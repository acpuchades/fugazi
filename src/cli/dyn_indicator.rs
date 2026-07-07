//! Runtime-typed indicator handle, unified across the CLI.
//!
//! The core `fugazi` indicator layer is statically composed: `Ema<S>`,
//! `Gt<L, R>`, `Combine<L, R, Op>` and so on are distinct types parameterised
//! by their sources, and the compiler enforces `Input`/`Output` compatibility
//! when they nest. A YAML-driven builder, by contrast, learns the shape of the
//! indicator tree only at runtime and needs one common return type it can
//! produce from every match arm and nest into the next.
//!
//! [`DynIndicator`] is that common type — a **runtime-typed** trait object
//! carrying its own [`input_type`](DynIndicator::input_type) /
//! [`output_type`](DynIndicator::output_type) descriptors, exchanging
//! [`DynValue`] payloads (`Real | Bool | Atom | Candle`) on every `update`.
//! Concrete library indicators are wrapped once by [`Adapter`] to appear as
//! `DynIndicator`s; the [`AsReal`] / [`AsBool`] typed views cross back the
//! other way so a boxed handle can drop into a library constructor.

use std::fmt;
use std::sync::Arc;

use fugazi::Indicator;
use fugazi::types::{Atom, Candle, Real};

// ---------------------------------------------------------------------------
// Payload enum + type descriptor
// ---------------------------------------------------------------------------

/// The runtime-typed payload a [`DynIndicator`] exchanges. One variant per
/// concrete carrier the CLI's indicator vocabulary produces / consumes.
///
/// `Real` and `Bool` are `Copy`; `Atom`, `Candle`, and `Str` are not, so
/// `DynValue` itself is only `Clone`.
#[derive(Debug, Clone)]
pub enum DynValue {
    Real(Real),
    Bool(bool),
    Atom(Atom),
    Candle(Candle),
    Str(Arc<str>),
}

// `Atom` doesn't implement `PartialEq` (the overlay `Arc`s aren't compared by
// the library), but the CLI's test helpers still need to assert on `DynValue`.
// Compare the scalar variants exactly, reduce `Atom`/`Candle` payloads to
// their candle-field equality (dropping overlays for the atom case), and
// compare `Str` payloads by their string contents.
impl PartialEq for DynValue {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (DynValue::Real(a), DynValue::Real(b)) => a == b,
            (DynValue::Bool(a), DynValue::Bool(b)) => a == b,
            (DynValue::Candle(a), DynValue::Candle(b)) => a == b,
            (DynValue::Atom(a), DynValue::Atom(b)) => a.candle == b.candle,
            (DynValue::Str(a), DynValue::Str(b)) => a.as_ref() == b.as_ref(),
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
}

impl fmt::Display for DynType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DynType::Real => f.write_str("Real"),
            DynType::Bool => f.write_str("Bool"),
            DynType::Atom => f.write_str("Atom"),
            DynType::Candle => f.write_str("Candle"),
            DynType::Str => f.write_str("Str"),
        }
    }
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

impl TryFrom<DynValue> for Real {
    type Error = DynType;
    fn try_from(v: DynValue) -> Result<Real, DynType> {
        match v {
            DynValue::Real(x) => Ok(x),
            DynValue::Bool(_) => Err(DynType::Bool),
            DynValue::Atom(_) => Err(DynType::Atom),
            DynValue::Candle(_) => Err(DynType::Candle),
            DynValue::Str(_) => Err(DynType::Str),
        }
    }
}
impl TryFrom<DynValue> for bool {
    type Error = DynType;
    fn try_from(v: DynValue) -> Result<bool, DynType> {
        match v {
            DynValue::Bool(x) => Ok(x),
            DynValue::Real(_) => Err(DynType::Real),
            DynValue::Atom(_) => Err(DynType::Atom),
            DynValue::Candle(_) => Err(DynType::Candle),
            DynValue::Str(_) => Err(DynType::Str),
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
            DynValue::Real(_) => Err(DynType::Real),
            DynValue::Bool(_) => Err(DynType::Bool),
            DynValue::Str(_) => Err(DynType::Str),
        }
    }
}
impl TryFrom<DynValue> for Candle {
    type Error = DynType;
    fn try_from(v: DynValue) -> Result<Candle, DynType> {
        match v {
            DynValue::Candle(x) => Ok(x),
            DynValue::Real(_) => Err(DynType::Real),
            DynValue::Bool(_) => Err(DynType::Bool),
            DynValue::Atom(_) => Err(DynType::Atom),
            DynValue::Str(_) => Err(DynType::Str),
        }
    }
}
impl TryFrom<DynValue> for Arc<str> {
    type Error = DynType;
    fn try_from(v: DynValue) -> Result<Arc<str>, DynType> {
        match v {
            DynValue::Str(s) => Ok(s),
            DynValue::Real(_) => Err(DynType::Real),
            DynValue::Bool(_) => Err(DynType::Bool),
            DynValue::Atom(_) => Err(DynType::Atom),
            DynValue::Candle(_) => Err(DynType::Candle),
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

// ---------------------------------------------------------------------------
// The runtime-typed trait + boxed handle
// ---------------------------------------------------------------------------

/// A runtime-typed [`Indicator`]-like object exchanging [`DynValue`] payloads.
///
/// Any concrete library `Indicator<Input = X, Output = Y>` where `X` /
/// `Y ∈ { Real, bool, Candle }` becomes a `DynIndicator` via the [`Adapter`]
/// blanket. To feed a `Box<dyn DynIndicator>` back into a library constructor
/// use the [`AsReal`] / [`AsBool`] typed views. Payload projection at
/// consumer sites is via `TryFrom<DynValue>` (the invariant is checked at
/// spec-build time, so the unwrap arm is unreachable).
pub trait DynIndicator {
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
    I: Indicator<Input = X, Output = Y> + Clone + 'static,
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
    I: Indicator<Input = X, Output = Y> + Clone + 'static,
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
/// a [`Resample`](fugazi::indicators::Resample) that emits every N base bars)
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
    let ok = outer.output_type() == inner.input_type()
        || (outer.output_type() == DynType::Candle && inner.input_type() == DynType::Atom);
    assert!(
        ok,
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
/// the runtime twin of [`Unstable`](fugazi::indicators::Unstable). Use to opt a
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
// Indicator<Input=Atom, Output=X> so it can drop into library constructors
// (Ema::new(source, period), IndicatorExt::gt(...), SingleAssetStrategy slots).
// ---------------------------------------------------------------------------

/// Views a `Box<dyn DynIndicator>` with `output_type == Real` as a library
/// `Indicator<Input = Atom, Output = Real>` — the shape every source-side
/// library constructor (Ema, Sma, arithmetic ops, comparisons, …) expects.
///
/// # Panics
/// [`new`](Self::new) panics if `inner.input_type() != Atom` or
/// `inner.output_type() != Real`; the recursive spec builder enforces both at
/// construction, so the unwrap arms in `update`/`value` are unreachable in
/// practice.
#[derive(Clone)]
pub struct AsReal(Box<dyn DynIndicator>);

impl AsReal {
    pub fn new(inner: Box<dyn DynIndicator>) -> Self {
        assert_eq!(
            inner.input_type(),
            DynType::Atom,
            "AsReal requires an Atom-input DynIndicator"
        );
        assert_eq!(
            inner.output_type(),
            DynType::Real,
            "AsReal requires a Real-output DynIndicator"
        );
        Self(inner)
    }
}

impl Indicator for AsReal {
    type Input = Atom;
    type Output = Real;
    fn update(&mut self, atom: Atom) -> Option<Real> {
        match self.0.update(DynValue::Atom(atom))? {
            DynValue::Real(x) => Some(x),
            other => unreachable!("AsReal received {other:?} but was built for Real output"),
        }
    }
    fn value(&self) -> Option<Real> {
        match self.0.value()? {
            DynValue::Real(x) => Some(x),
            other => unreachable!("AsReal held {other:?} but was built for Real output"),
        }
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

/// Views a `Box<dyn DynIndicator>` with `output_type == Bool` as a library
/// `Indicator<Input = Atom, Output = bool>` — i.e. a
/// [`Signal`](fugazi::Signal).
///
/// # Panics
/// [`new`](Self::new) panics if `inner.input_type() != Atom` or
/// `inner.output_type() != Bool`.
#[derive(Clone)]
pub struct AsBool(Box<dyn DynIndicator>);

impl AsBool {
    pub fn new(inner: Box<dyn DynIndicator>) -> Self {
        assert_eq!(
            inner.input_type(),
            DynType::Atom,
            "AsBool requires an Atom-input DynIndicator"
        );
        assert_eq!(
            inner.output_type(),
            DynType::Bool,
            "AsBool requires a Bool-output DynIndicator"
        );
        Self(inner)
    }
}

impl Indicator for AsBool {
    type Input = Atom;
    type Output = bool;
    fn update(&mut self, atom: Atom) -> Option<bool> {
        match self.0.update(DynValue::Atom(atom))? {
            DynValue::Bool(b) => Some(b),
            other => unreachable!("AsBool received {other:?} but was built for Bool output"),
        }
    }
    fn value(&self) -> Option<bool> {
        match self.0.value()? {
            DynValue::Bool(b) => Some(b),
            other => unreachable!("AsBool held {other:?} but was built for Bool output"),
        }
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

/// Views a `Box<dyn DynIndicator>` with `output_type == Candle` as a library
/// `Indicator<Input = Atom, Output = Candle>` — the shape a bar indicator
/// (`Atr`, `Adx`, `Obv`, …) expects as its `source` after the source-generic
/// refactor.
///
/// # Panics
/// [`new`](Self::new) panics if `inner.input_type() != Atom` or
/// `inner.output_type() != Candle`.
#[derive(Clone)]
pub struct AsCandle(Box<dyn DynIndicator>);

impl AsCandle {
    pub fn new(inner: Box<dyn DynIndicator>) -> Self {
        assert_eq!(
            inner.input_type(),
            DynType::Atom,
            "AsCandle requires an Atom-input DynIndicator"
        );
        assert_eq!(
            inner.output_type(),
            DynType::Candle,
            "AsCandle requires a Candle-output DynIndicator"
        );
        Self(inner)
    }
}

impl Indicator for AsCandle {
    type Input = Atom;
    type Output = Candle;
    fn update(&mut self, atom: Atom) -> Option<Candle> {
        match self.0.update(DynValue::Atom(atom))? {
            DynValue::Candle(c) => Some(c),
            other => unreachable!("AsCandle received {other:?} but was built for Candle output"),
        }
    }
    fn value(&self) -> Option<Candle> {
        match self.0.value()? {
            DynValue::Candle(c) => Some(c),
            other => unreachable!("AsCandle held {other:?} but was built for Candle output"),
        }
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

/// Views a `Box<dyn DynIndicator>` with `output_type == Str` as a library
/// `Indicator<Input = Atom, Output = Arc<str>>` — the shape a
/// [`StrEq`](fugazi::indicators::StrEq) or any other string-consuming
/// combinator expects for its sources.
///
/// # Panics
/// [`new`](Self::new) panics if `inner.input_type() != Atom` or
/// `inner.output_type() != Str`.
#[derive(Clone)]
pub struct AsStr(Box<dyn DynIndicator>);

impl AsStr {
    pub fn new(inner: Box<dyn DynIndicator>) -> Self {
        assert_eq!(
            inner.input_type(),
            DynType::Atom,
            "AsStr requires an Atom-input DynIndicator"
        );
        assert_eq!(
            inner.output_type(),
            DynType::Str,
            "AsStr requires a Str-output DynIndicator"
        );
        Self(inner)
    }
}

impl Indicator for AsStr {
    type Input = Atom;
    type Output = Arc<str>;
    fn update(&mut self, atom: Atom) -> Option<Arc<str>> {
        match self.0.update(DynValue::Atom(atom))? {
            DynValue::Str(s) => Some(s),
            other => unreachable!("AsStr received {other:?} but was built for Str output"),
        }
    }
    fn value(&self) -> Option<Arc<str>> {
        match self.0.value()? {
            DynValue::Str(s) => Some(s),
            other => unreachable!("AsStr held {other:?} but was built for Str output"),
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use fugazi::indicators::{Current, Ema, Sma};

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
