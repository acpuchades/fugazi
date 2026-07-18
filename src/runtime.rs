//! Runtime-typed indicator handle for spec-driven builders.
//!
//! The core `fugazi` indicator layer is statically composed: `Ema<S>`,
//! `Gt<L, R>`, `Combine<L, R, Op>` and so on are distinct types parameterised
//! by their sources, and the compiler enforces `Input`/`Output` compatibility
//! when they nest. A YAML- or Python-driven builder, by contrast, learns the
//! shape of the indicator tree only at runtime and needs one common return
//! type it can produce from every match arm and nest into the next.
//!
//! [`DynIndicator`] is that common type — a **runtime-typed** trait object
//! carrying its own [`input_type`](DynIndicator::input_type) /
//! [`output_type`](DynIndicator::output_type) descriptors, exchanging
//! [`DynValue`] payloads (`Real | Bool | Atom | Candle | Str | Time |
//! Snapshot`) on every `update`. Concrete library indicators are wrapped once
//! by [`Adapter`] to appear as `DynIndicator`s; the [`AsReal`] / [`AsBool`] /
//! [`AsCandle`] / [`AsAtom`] / [`AsStr`] typed views cross back the other way
//! so a boxed handle can drop into a library constructor.
//!
//! Gated behind the `runtime` Cargo feature (default-on; implied by `cli`).
//! A pure-lib user with no YAML/JSON/Python surface doesn't need it and can
//! disable it via `default-features = false`.

use std::fmt;
use std::sync::Arc;

use crate::Indicator;
use crate::market::{Atom, Candle, Real};
use crate::snapshot::Snapshot;
use crate::time::Timestamp;

// ---------------------------------------------------------------------------
// Payload enum + type descriptor
// ---------------------------------------------------------------------------

/// The runtime-typed payload a [`DynIndicator`] exchanges. One variant per
/// concrete carrier the shared runtime-typed indicator vocabulary produces /
/// consumes.
///
/// `Real`, `Bool` and `Time` are `Copy`; `Atom`, `Candle`, `Str` and
/// `Snapshot` are not, so `DynValue` itself is only `Clone`.
///
/// The `Snapshot` variant is keyed by `String` — the symbol space YAML/JSON
/// specs and the Python bindings both produce is `String`-typed end-to-end.
#[derive(Debug, Clone)]
pub enum DynValue {
    Real(Real),
    Bool(bool),
    Atom(Atom),
    Candle(Candle),
    Str(Arc<str>),
    Time(Timestamp),
    Snapshot(Snapshot<String>),
}

// `Atom` doesn't implement `PartialEq` (the overlay `Arc`s aren't compared by
// the library), but downstream test helpers still need to assert on
// `DynValue`. Compare the scalar variants exactly, reduce `Atom`/`Candle`
// payloads to their candle-field equality (dropping overlays for the atom
// case), and compare `Str` payloads by their string contents. Snapshots are
// compared by their `(sym, freq, atom.candle)` tuples — the same "atoms by
// candle-fields" reduction as the standalone Atom case.
impl PartialEq for DynValue {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (DynValue::Real(a), DynValue::Real(b)) => a == b,
            (DynValue::Bool(a), DynValue::Bool(b)) => a == b,
            (DynValue::Candle(a), DynValue::Candle(b)) => a == b,
            (DynValue::Atom(a), DynValue::Atom(b)) => a.candle == b.candle,
            (DynValue::Str(a), DynValue::Str(b)) => a.as_ref() == b.as_ref(),
            (DynValue::Time(a), DynValue::Time(b)) => a == b,
            (DynValue::Snapshot(a), DynValue::Snapshot(b)) => {
                a.len() == b.len()
                    && a.iter().zip(b.iter()).all(|((sa, fa, aa), (sb, fb, ab))| {
                        sa == sb && fa == fb && aa.candle == ab.candle
                    })
            }
            _ => false,
        }
    }
}

/// The runtime tag on a [`DynValue`] — used to check
/// [`DynIndicator::input_type`] / [`output_type`](DynIndicator::output_type)
/// compatibility at spec-build time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DynType {
    Real,
    Bool,
    Atom,
    Candle,
    Str,
    Time,
    Snapshot,
}

impl DynValue {
    /// The runtime [`DynType`] tag of the payload actually carried. The
    /// inverse of the compile-time `<T as TypeOf>::TYPE`; centralising it here
    /// means the [`TryFrom<DynValue>`] impls can spell their error arm as one
    /// catch-all instead of listing every non-matching variant.
    pub fn dyn_type(&self) -> DynType {
        match self {
            DynValue::Real(_) => DynType::Real,
            DynValue::Bool(_) => DynType::Bool,
            DynValue::Atom(_) => DynType::Atom,
            DynValue::Candle(_) => DynType::Candle,
            DynValue::Str(_) => DynType::Str,
            DynValue::Time(_) => DynType::Time,
            DynValue::Snapshot(_) => DynType::Snapshot,
        }
    }
}

impl fmt::Display for DynType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DynType::Real => f.write_str("Real"),
            DynType::Bool => f.write_str("Bool"),
            DynType::Atom => f.write_str("Atom"),
            DynType::Candle => f.write_str("Candle"),
            DynType::Str => f.write_str("Str"),
            DynType::Time => f.write_str("Time"),
            DynType::Snapshot => f.write_str("Snapshot"),
        }
    }
}

/// Report whether a [`DynValue`] tagged `from` can be consumed by a
/// [`DynIndicator`] with `input_type() == into`. Returns `true` when the tags
/// match exactly, or when a well-defined [`TryFrom<DynValue>`] lift bridges
/// them (`Candle → Atom`, `Atom → Snapshot`, `Candle → Snapshot`).
///
/// **Single source of truth for coercion compatibility.** Both this table
/// *and* the corresponding lift arms on the `TryFrom<DynValue>` impls (for
/// `Atom` and `Snapshot<String>`) list the same three lifts, and a lift-parity
/// test in this module holds them in sync — adding a new lift on either side
/// without the other fails that test.
///
/// A probing implementation (build a sentinel `DynValue` of `from`'s variant
/// and check whether the appropriate `TryFrom` returns `Ok`) would be more
/// self-consistent, but that would require default constructors for `Atom`
/// and `Candle` that don't exist and shouldn't be added just for this.
pub fn can_lift(from: DynType, into: DynType) -> bool {
    from == into
        || matches!(
            (from, into),
            (DynType::Candle, DynType::Atom)
                | (DynType::Atom, DynType::Snapshot)
                | (DynType::Candle, DynType::Snapshot)
        )
}

impl From<Real> for DynValue {
    fn from(v: Real) -> Self {
        DynValue::Real(v)
    }
}
impl From<bool> for DynValue {
    fn from(v: bool) -> Self {
        DynValue::Bool(v)
    }
}
impl From<Atom> for DynValue {
    fn from(v: Atom) -> Self {
        DynValue::Atom(v)
    }
}
impl From<Candle> for DynValue {
    fn from(v: Candle) -> Self {
        DynValue::Candle(v)
    }
}
impl From<Arc<str>> for DynValue {
    fn from(v: Arc<str>) -> Self {
        DynValue::Str(v)
    }
}
impl From<Timestamp> for DynValue {
    fn from(v: Timestamp) -> Self {
        DynValue::Time(v)
    }
}
impl From<Snapshot<String>> for DynValue {
    fn from(v: Snapshot<String>) -> Self {
        DynValue::Snapshot(v)
    }
}

impl TryFrom<DynValue> for Real {
    type Error = DynType;
    fn try_from(v: DynValue) -> Result<Real, DynType> {
        match v {
            DynValue::Real(x) => Ok(x),
            other => Err(other.dyn_type()),
        }
    }
}
impl TryFrom<DynValue> for bool {
    type Error = DynType;
    fn try_from(v: DynValue) -> Result<bool, DynType> {
        match v {
            DynValue::Bool(x) => Ok(x),
            other => Err(other.dyn_type()),
        }
    }
}
impl TryFrom<DynValue> for Atom {
    type Error = DynType;
    fn try_from(v: DynValue) -> Result<Atom, DynType> {
        match v {
            DynValue::Atom(x) => Ok(x),
            // A raw Candle lifts trivially into an Atom with no overlays —
            // this is the key that lets a Resample's Candle output feed a
            // downstream Atom-input source without an explicit lift adapter.
            DynValue::Candle(c) => Ok(c.into()),
            other => Err(other.dyn_type()),
        }
    }
}
impl TryFrom<DynValue> for Candle {
    type Error = DynType;
    fn try_from(v: DynValue) -> Result<Candle, DynType> {
        match v {
            DynValue::Candle(x) => Ok(x),
            other => Err(other.dyn_type()),
        }
    }
}
impl TryFrom<DynValue> for Arc<str> {
    type Error = DynType;
    fn try_from(v: DynValue) -> Result<Arc<str>, DynType> {
        match v {
            DynValue::Str(s) => Ok(s),
            other => Err(other.dyn_type()),
        }
    }
}
impl TryFrom<DynValue> for Timestamp {
    type Error = DynType;
    fn try_from(v: DynValue) -> Result<Timestamp, DynType> {
        match v {
            DynValue::Time(t) => Ok(t),
            other => Err(other.dyn_type()),
        }
    }
}
impl TryFrom<DynValue> for Snapshot<String> {
    type Error = DynType;
    fn try_from(v: DynValue) -> Result<Snapshot<String>, DynType> {
        match v {
            DynValue::Snapshot(s) => Ok(s),
            // A Candle or Atom lifts into an untagged size-1 snapshot — the
            // key that lets a Resample's Candle output (or any Atom-emitting
            // source's output) feed a downstream Snapshot-rooted chain via
            // the sole-atom unpack that empty-selector `!pick` uses.
            DynValue::Candle(c) => Ok(Snapshot::<String>::of_atom(c.into())),
            DynValue::Atom(a) => Ok(Snapshot::<String>::of_atom(a)),
            other => Err(other.dyn_type()),
        }
    }
}

/// Maps a concrete carrier type (`Real`, `bool`, `Atom`, `Candle`, `Arc<str>`)
/// back to its [`DynType`] tag — the compile-time counterpart of the runtime
/// descriptor the [`Adapter`] blanket uses to fill in `input_type()` /
/// `output_type()`.
pub trait TypeOf {
    const TYPE: DynType;
}
impl TypeOf for Real {
    const TYPE: DynType = DynType::Real;
}
impl TypeOf for bool {
    const TYPE: DynType = DynType::Bool;
}
impl TypeOf for Atom {
    const TYPE: DynType = DynType::Atom;
}
impl TypeOf for Candle {
    const TYPE: DynType = DynType::Candle;
}
impl TypeOf for Arc<str> {
    const TYPE: DynType = DynType::Str;
}
impl TypeOf for Timestamp {
    const TYPE: DynType = DynType::Time;
}
impl TypeOf for Snapshot<String> {
    const TYPE: DynType = DynType::Snapshot;
}

// ---------------------------------------------------------------------------
// The runtime-typed trait + boxed handle
// ---------------------------------------------------------------------------

/// A runtime-typed [`Indicator`]-like object exchanging [`DynValue`] payloads.
///
/// Any concrete library `Indicator<Input = X, Output = Y>` where `X` /
/// `Y ∈ { Real, bool, Candle, Atom, Arc<str>, Timestamp, Snapshot<String> }`
/// becomes a `DynIndicator` via the [`Adapter`] blanket. To feed a
/// `Box<dyn DynIndicator>` back into a library constructor use the [`AsReal`] /
/// [`AsBool`] / [`AsCandle`] / [`AsAtom`] / [`AsStr`] typed views. Payload
/// projection at consumer sites is via `TryFrom<DynValue>` (the invariant is
/// checked at spec-build time, so the unwrap arm is unreachable).
pub trait DynIndicator: Send + Sync {
    fn input_type(&self) -> DynType;
    fn output_type(&self) -> DynType;
    fn update(&mut self, input: DynValue) -> Option<DynValue>;
    fn value(&self) -> Option<DynValue>;
    fn warm_up_period(&self) -> usize;
    fn unstable_period(&self) -> usize;
    fn stable_period(&self) -> usize {
        self.warm_up_period()
            .saturating_add(self.unstable_period())
    }
    fn reset(&mut self);
    /// Deep-clone the box. Threads `Clone` through the trait object the way the
    /// older `CloneableValue` supertrait did — needed because some concrete
    /// indicators internally clone their source (multi-output component
    /// accessors, `Hma`, `crosses_above`), so a `DynIndicator` must itself be
    /// clonable to slot into their construction.
    fn dyn_clone(&self) -> Box<dyn DynIndicator>;
}

impl Clone for Box<dyn DynIndicator> {
    fn clone(&self) -> Box<dyn DynIndicator> {
        (**self).dyn_clone()
    }
}

/// [`DynIndicator`] plus `Send + Sync` and a `Send + Sync`-preserving deep
/// clone. The base [`DynIndicator`] trait deliberately doesn't require these
/// autotraits — some concrete library indicators (`PositionField`, `BookField`)
/// hold `Rc<RefCell<…>>` state and can't satisfy them, and the CLI's spec
/// builder wraps those alongside the rest. Downstream callers that *do* need
/// autotrait-preserving type erasure (pyo3 pyclasses require `Send + Sync` on
/// every field) reach for this subtrait via [`wrap_sync`] instead of [`wrap`].
///
/// The blanket impl fires for every `T: DynIndicator + Clone + Send + Sync +
/// 'static`, so `Adapter<I>` picks it up automatically when `I` is itself
/// `Send + Sync` — which every stateless indicator (`Ema`, `Sma`, `Rsi`,
/// `Combine`, …) is trivially.
pub trait DynIndicatorSync: DynIndicator + Send + Sync {
    fn dyn_clone_sync(&self) -> Box<dyn DynIndicatorSync>;
}

impl<T> DynIndicatorSync for T
where
    T: DynIndicator + Clone + Send + Sync + 'static,
{
    fn dyn_clone_sync(&self) -> Box<dyn DynIndicatorSync> {
        Box::new(self.clone())
    }
}

impl Clone for Box<dyn DynIndicatorSync> {
    fn clone(&self) -> Box<dyn DynIndicatorSync> {
        (**self).dyn_clone_sync()
    }
}

// ---------------------------------------------------------------------------
// Adapter: concrete Indicator → DynIndicator
// ---------------------------------------------------------------------------

/// Wraps a concrete library [`Indicator`] as a [`DynIndicator`].
///
/// One blanket impl over every `I: Indicator<Input = X, Output = Y>` where
/// `X: TryFrom<DynValue, Error = DynType> + TypeOf` and
/// `Y: Into<DynValue> + Clone + TypeOf`. `Y` is `Clone` (not `Copy`) because
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

impl<I, X, Y> DynIndicator for Adapter<I>
where
    I: Indicator<Input = X, Output = Y> + Clone + Send + Sync + 'static,
    X: TryFrom<DynValue, Error = DynType> + TypeOf,
    Y: Into<DynValue> + Clone + TypeOf,
{
    fn input_type(&self) -> DynType {
        X::TYPE
    }
    fn output_type(&self) -> DynType {
        Y::TYPE
    }
    fn update(&mut self, input: DynValue) -> Option<DynValue> {
        let x = X::try_from(input).unwrap_or_else(|got| {
            panic!(
                "DynIndicator input type mismatch: expected {}, got {}",
                X::TYPE,
                got
            )
        });
        self.inner.update(x).map(Into::into)
    }
    fn value(&self) -> Option<DynValue> {
        self.inner.value().map(Into::into)
    }
    fn warm_up_period(&self) -> usize {
        self.inner.warm_up_period()
    }
    fn unstable_period(&self) -> usize {
        self.inner.unstable_period()
    }
    fn reset(&mut self) {
        self.inner.reset();
    }
    fn dyn_clone(&self) -> Box<dyn DynIndicator> {
        Box::new(self.clone())
    }
}

/// Wrap a concrete indicator into a boxed [`DynIndicator`].
pub fn wrap<I, X, Y>(inner: I) -> Box<dyn DynIndicator>
where
    I: Indicator<Input = X, Output = Y> + Clone + Send + Sync + 'static,
    X: TryFrom<DynValue, Error = DynType> + TypeOf,
    Y: Into<DynValue> + Clone + TypeOf,
{
    Box::new(Adapter::new(inner))
}

/// Wrap a concrete indicator into a boxed [`DynIndicatorSync`] — the
/// autotrait-preserving twin of [`wrap`] for callers that need `Send + Sync`
/// (pyo3 pyclasses, thread-crossing state).
pub fn wrap_sync<I, X, Y>(inner: I) -> Box<dyn DynIndicatorSync>
where
    I: Indicator<Input = X, Output = Y> + Clone + Send + Sync + 'static,
    X: TryFrom<DynValue, Error = DynType> + TypeOf,
    Y: Into<DynValue> + Clone + TypeOf,
{
    Box::new(Adapter::new(inner))
}

// ---------------------------------------------------------------------------
// chain: runtime-typed composition of two DynIndicators
// ---------------------------------------------------------------------------

/// Compose two [`DynIndicator`]s so that `outer`'s output feeds `inner`'s
/// input at runtime. The returned box has `input_type() =
/// outer.input_type()` and `output_type() = inner.output_type()`. `inner`
/// only advances on ticks where `outer` emits `Some`, so a slow `outer` (e.g.
/// a [`Resample`](crate::indicators::Resample) that emits every N base bars)
/// naturally sub-samples the `inner`.
///
/// The composed warm-up and unstable-period are the plain sum of the two —
/// the same arithmetic the library uses when composing statically, in
/// `outer`-emission units for `inner` — so `!stable { signal }` (or any
/// downstream reader of `stable_period()`) is on the same convention as a
/// pure-library composition and doesn't get base-bar-scaled for free.
///
/// # Panics
/// If `outer.output_type() != inner.input_type()`, at construction — the
/// recursive spec builder guarantees compatible types, so this is a hard bug
/// if ever hit.
pub fn chain(outer: Box<dyn DynIndicator>, inner: Box<dyn DynIndicator>) -> Box<dyn DynIndicator> {
    assert!(
        can_lift(outer.output_type(), inner.input_type()),
        "chain: outer output type ({}) doesn't match inner input type ({})",
        outer.output_type(),
        inner.input_type(),
    );
    Box::new(Chain {
        outer,
        inner,
        value: None,
    })
}

struct Chain {
    outer: Box<dyn DynIndicator>,
    inner: Box<dyn DynIndicator>,
    value: Option<DynValue>,
}

impl DynIndicator for Chain {
    fn input_type(&self) -> DynType {
        self.outer.input_type()
    }
    fn output_type(&self) -> DynType {
        self.inner.output_type()
    }
    fn update(&mut self, x: DynValue) -> Option<DynValue> {
        self.value = match self.outer.update(x) {
            Some(y) => self.inner.update(y),
            None => None,
        };
        self.value.clone()
    }
    fn value(&self) -> Option<DynValue> {
        self.value.clone()
    }
    fn warm_up_period(&self) -> usize {
        // Plain library-style composition: outer needs its warm-up, then
        // inner needs `inner.warm_up_period() - 1` more outer-emissions (one
        // coincides with outer's first emit). The unit is outer-samples for
        // outer's part and outer-emissions for inner's part, i.e. the same
        // undifferentiated arithmetic as `Ema::new(Resample.close(), P)` in
        // pure Rust.
        self.outer
            .warm_up_period()
            .saturating_add(self.inner.warm_up_period().saturating_sub(1))
    }
    fn unstable_period(&self) -> usize {
        self.outer
            .unstable_period()
            .saturating_add(self.inner.unstable_period())
    }
    fn reset(&mut self) {
        self.outer.reset();
        self.inner.reset();
        self.value = None;
    }
    fn dyn_clone(&self) -> Box<dyn DynIndicator> {
        Box::new(Chain {
            outer: self.outer.dyn_clone(),
            inner: self.inner.dyn_clone(),
            value: self.value.clone(),
        })
    }
}

// ---------------------------------------------------------------------------
// unstable_wrap: runtime-typed passthrough that zeroes unstable_period()
// (mirrors the library's Unstable)
// ---------------------------------------------------------------------------

/// A [`DynIndicator`] wrapper that forwards every method to `inner` *except*
/// [`unstable_period`](DynIndicator::unstable_period), which it forces to `0` —
/// the runtime twin of [`Unstable`](crate::indicators::Unstable). Use to opt a
/// subtree out of the strategy-readiness wait for its IIR settling tail.
pub fn unstable_wrap(inner: Box<dyn DynIndicator>) -> Box<dyn DynIndicator> {
    Box::new(UnstableWrap { inner })
}

struct UnstableWrap {
    inner: Box<dyn DynIndicator>,
}

impl DynIndicator for UnstableWrap {
    fn input_type(&self) -> DynType {
        self.inner.input_type()
    }
    fn output_type(&self) -> DynType {
        self.inner.output_type()
    }
    fn update(&mut self, x: DynValue) -> Option<DynValue> {
        self.inner.update(x)
    }
    fn value(&self) -> Option<DynValue> {
        self.inner.value()
    }
    fn warm_up_period(&self) -> usize {
        self.inner.warm_up_period()
    }
    fn unstable_period(&self) -> usize {
        0
    }
    fn reset(&mut self) {
        self.inner.reset();
    }
    fn dyn_clone(&self) -> Box<dyn DynIndicator> {
        Box::new(UnstableWrap {
            inner: self.inner.dyn_clone(),
        })
    }
}

// ---------------------------------------------------------------------------
// Typed views: reconstitute a Box<dyn DynIndicator> as a library-typed
// Indicator<Input=Snapshot<String>, Output=Out> so it can drop into library
// constructors (Ema::new(source, period), IndicatorExt::gt(...),
// SingleAssetStrategy slots). Callers whose whole indicator chain is
// snapshot-rooted — every atom-input leaf is wrapped in a `!pick` on parse,
// so every DynIndicator in the tree consumes `Snapshot<String>` — use these.
//
// One generic [`As<Out>`] carrier covers every supported output type; the
// per-type names ([`AsReal`], [`AsBool`], [`AsCandle`], [`AsAtom`], [`AsStr`])
// are type aliases over it.
// ---------------------------------------------------------------------------

/// Views a `Box<dyn DynIndicator>` with `output_type == Out::TYPE` as a
/// library-typed `Indicator<Input = Snapshot<String>, Output = Out>` so it
/// drops into any source-wrapping library constructor (Ema, Sma, arithmetic
/// ops, comparisons, `SingleAssetStrategy` slots).
///
/// # Panics
/// [`new`](Self::new) panics if `inner.input_type() != Snapshot` or
/// `inner.output_type() != Out::TYPE`; the recursive spec builder enforces
/// both at construction, so the unwrap arms in `update`/`value` are
/// unreachable in practice.
pub struct As<Out>(Box<dyn DynIndicator>, std::marker::PhantomData<fn() -> Out>);

impl<Out: TypeOf> As<Out> {
    pub fn new(inner: Box<dyn DynIndicator>) -> Self {
        assert_eq!(
            inner.input_type(),
            DynType::Snapshot,
            "As<{}> requires a Snapshot-input DynIndicator",
            Out::TYPE,
        );
        assert_eq!(
            inner.output_type(),
            Out::TYPE,
            "As<{}> requires a {}-output DynIndicator",
            Out::TYPE,
            Out::TYPE,
        );
        Self(inner, std::marker::PhantomData)
    }
}

impl<Out> Clone for As<Out> {
    fn clone(&self) -> Self {
        Self(self.0.clone(), std::marker::PhantomData)
    }
}

impl<Out> Indicator for As<Out>
where
    Out: TypeOf + TryFrom<DynValue, Error = DynType> + Clone,
{
    type Input = Snapshot<String>;
    type Output = Out;
    fn update(&mut self, snap: Snapshot<String>) -> Option<Out> {
        let payload = self.0.update(DynValue::Snapshot(snap))?;
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
    fn warm_up_period(&self) -> usize {
        self.0.warm_up_period()
    }
    fn unstable_period(&self) -> usize {
        self.0.unstable_period()
    }
    fn reset(&mut self) {
        self.0.reset();
    }
}

/// `Real`-output typed view — the shape every source-side library constructor
/// (Ema, Sma, arithmetic ops, comparisons, …) expects once the caller's
/// leaves have been rooted through `Pick`.
pub type AsReal = As<Real>;

/// `bool`-output typed view — i.e. a
/// [`Signal<Snapshot<String>>`](crate::Signal).
pub type AsBool = As<bool>;

/// `Candle`-output typed view — the shape a bar indicator (`Atr`, `Adx`,
/// `Obv`, …) expects as its `source` after the source-generic refactor.
pub type AsCandle = As<Candle>;

/// `Atom`-output typed view — the atom-emitting bridge every source-generic
/// atom-input leaf (`Close::of(source)`, `Year::of(source)`,
/// `Atr::new(CurrentBar::of(source), period)`, …) uses. The typical concrete
/// source is `Pick::<String>::new()` — the empty selector's
/// `Snapshot::sole_atom` unpack — but any snapshot-rooted atom-emitting
/// chain works.
///
/// Not currently constructed by the CLI spec builder — every leaf that would
/// want it (`!close`, `!year`, `!current`, …) already builds itself with
/// `Pick::<String>::new()` baked in, so no intermediate `AsAtom` is
/// needed. Kept for completeness so a future `!pick { symbol, freq }`
/// ExprSpec variant can produce an atom-emitting DynIndicator and drop
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
        assert_eq!(Real::try_from(DynValue::from(1.5_f64)).unwrap(), 1.5);
        assert!(bool::try_from(DynValue::from(true)).unwrap());
        let c = Candle::new(1.0, 2.0, 0.5, 1.5, 100.0);
        assert_eq!(Candle::try_from(DynValue::from(c)).unwrap(), c);

        // Type mismatch carries the actual variant tag for diagnostics.
        assert_eq!(
            Real::try_from(DynValue::from(true)).unwrap_err(),
            DynType::Bool
        );
    }

    #[test]
    fn adapter_reports_types_and_forwards_payload() {
        let mut sma = wrap(Sma::new(Current::close(), 3));
        assert_eq!(sma.input_type(), DynType::Atom);
        assert_eq!(sma.output_type(), DynType::Real);

        assert_eq!(sma.update(DynValue::Atom(bar(1.0).into())), None);
        assert_eq!(sma.update(DynValue::Atom(bar(2.0).into())), None);
        assert_eq!(
            sma.update(DynValue::Atom(bar(3.0).into())),
            Some(DynValue::Real(2.0))
        );
    }

    #[test]
    fn unstable_wrap_zeroes_unstable_but_forwards_output() {
        let raw = Ema::new(Current::close(), 3);
        let warm = raw.warm_up_period();
        let settle = raw.unstable_period();
        assert!(settle > 0, "Ema-3 should have a real unstable tail");

        let mut wrapped = unstable_wrap(wrap(Ema::new(Current::close(), 3)));
        let mut plain = wrap(Ema::new(Current::close(), 3));
        assert_eq!(wrapped.input_type(), DynType::Atom);
        assert_eq!(wrapped.output_type(), DynType::Real);
        assert_eq!(wrapped.warm_up_period(), warm);
        assert_eq!(wrapped.unstable_period(), 0);
        assert_eq!(wrapped.stable_period(), warm);

        let bar = |v: Real| DynValue::Atom(Candle::new(v, v, v, v, 0.0).into());
        for i in 1..=5 {
            assert_eq!(wrapped.update(bar(i as Real)), plain.update(bar(i as Real)));
        }
    }

    #[test]
    fn stable_period_defaults_to_warm_up_plus_unstable() {
        let ema = wrap(Ema::new(Current::close(), 3));
        assert_eq!(
            ema.stable_period(),
            ema.warm_up_period() + ema.unstable_period()
        );
    }

    #[test]
    fn can_lift_matches_try_from_impls() {
        // For every (from, into) pair, verify `can_lift` agrees with what the
        // `TryFrom<DynValue>` impls actually accept. `can_lift` is the table
        // `chain()` consults at construction; a drift between the table and the
        // real lift semantics would either accept a chain that panics on the
        // first tick (if `can_lift` said yes but `TryFrom` says no) or refuse a
        // chain that would have worked (if the reverse).
        //
        // The sample values here are the sentinels the `TryFrom` impls actually
        // exercise. `Snapshot`, `Str`, `Time` are self-only, `Real`/`Bool` are
        // self-only, `Candle` lifts to `Atom` and `Snapshot`, `Atom` lifts to
        // `Snapshot`.
        let sample = |t: DynType| -> DynValue {
            match t {
                DynType::Real => DynValue::Real(1.0),
                DynType::Bool => DynValue::Bool(true),
                DynType::Candle => DynValue::Candle(bar(1.0)),
                DynType::Atom => DynValue::Atom(bar(1.0).into()),
                DynType::Str => DynValue::Str(Arc::from("x")),
                DynType::Time => DynValue::Time(Timestamp(0)),
                DynType::Snapshot => DynValue::Snapshot(crate::snapshot::Snapshot::new()),
            }
        };
        let try_into_ok = |v: DynValue, into: DynType| -> bool {
            match into {
                DynType::Real => Real::try_from(v).is_ok(),
                DynType::Bool => bool::try_from(v).is_ok(),
                DynType::Atom => Atom::try_from(v).is_ok(),
                DynType::Candle => Candle::try_from(v).is_ok(),
                DynType::Str => Arc::<str>::try_from(v).is_ok(),
                DynType::Time => Timestamp::try_from(v).is_ok(),
                DynType::Snapshot => crate::snapshot::Snapshot::<String>::try_from(v).is_ok(),
            }
        };
        let all = [
            DynType::Real,
            DynType::Bool,
            DynType::Candle,
            DynType::Atom,
            DynType::Str,
            DynType::Time,
            DynType::Snapshot,
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
        assert_eq!(sma.update(DynValue::Atom(bar(1.0).into())), None);
        assert_eq!(clone.update(DynValue::Atom(bar(10.0).into())), None);
        assert_eq!(
            sma.update(DynValue::Atom(bar(3.0).into())),
            Some(DynValue::Real(2.0))
        );
        assert_eq!(
            clone.update(DynValue::Atom(bar(20.0).into())),
            Some(DynValue::Real(15.0))
        );
    }

    #[test]
    fn str_payload_roundtrips_through_dynvalue() {
        let s: Arc<str> = Arc::from("bull");
        let v: DynValue = s.clone().into();
        assert_eq!(v, DynValue::Str(Arc::from("bull")));
        let back: Arc<str> = v.try_into().unwrap();
        assert_eq!(back.as_ref(), "bull");
        // Mismatch surfaces the actual variant tag.
        assert_eq!(
            Arc::<str>::try_from(DynValue::from(1.0_f64)).unwrap_err(),
            DynType::Real,
        );
    }
}
