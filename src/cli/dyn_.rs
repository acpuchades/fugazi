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
//! [`DynValue`] payloads (`Real | Bool | Candle`) on every `update`. Concrete
//! library indicators are wrapped once by [`Adapter`] to appear as
//! `DynIndicator`s; the [`AsReal`] / [`AsBool`] typed views cross back the
//! other way so a boxed handle can drop into a library constructor.

use std::fmt;

use fugazi::Indicator;
use fugazi::types::{Candle, Real};

// ---------------------------------------------------------------------------
// Payload enum + type descriptor
// ---------------------------------------------------------------------------

/// The runtime-typed payload a [`DynIndicator`] exchanges. One variant per
/// concrete carrier the CLI's indicator vocabulary produces / consumes.
///
/// All three variants are [`Copy`], so passing `DynValue` by value stays cheap.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DynValue {
    Real(Real),
    Bool(bool),
    Candle(Candle),
}

/// The runtime tag on a [`DynValue`] — used to check
/// [`DynIndicator::input_type`] / [`output_type`](DynIndicator::output_type)
/// compatibility at spec-build time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DynType {
    Real,
    Bool,
    Candle,
}

impl fmt::Display for DynType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DynType::Real => f.write_str("Real"),
            DynType::Bool => f.write_str("Bool"),
            DynType::Candle => f.write_str("Candle"),
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
impl From<Candle> for DynValue {
    fn from(v: Candle) -> Self {
        DynValue::Candle(v)
    }
}

impl TryFrom<DynValue> for Real {
    type Error = DynType;
    fn try_from(v: DynValue) -> Result<Real, DynType> {
        match v {
            DynValue::Real(x) => Ok(x),
            DynValue::Bool(_) => Err(DynType::Bool),
            DynValue::Candle(_) => Err(DynType::Candle),
        }
    }
}
impl TryFrom<DynValue> for bool {
    type Error = DynType;
    fn try_from(v: DynValue) -> Result<bool, DynType> {
        match v {
            DynValue::Bool(x) => Ok(x),
            DynValue::Real(_) => Err(DynType::Real),
            DynValue::Candle(_) => Err(DynType::Candle),
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
        }
    }
}

/// Maps a concrete carrier type (`Real`, `bool`, `Candle`) back to its
/// [`DynType`] tag — the compile-time counterpart of the runtime descriptor
/// the [`Adapter`] blanket uses to fill in `input_type()` / `output_type()`.
pub trait TypeOf {
    const TYPE: DynType;
}
impl TypeOf for Real {
    const TYPE: DynType = DynType::Real;
}
impl TypeOf for bool {
    const TYPE: DynType = DynType::Bool;
}
impl TypeOf for Candle {
    const TYPE: DynType = DynType::Candle;
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
/// `Y: Into<DynValue> + Copy + TypeOf`. The `Copy` on `Y` matches the current
/// carrier surface (`Real`, `bool`, `Candle` are all `Copy`).
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
    Y: Into<DynValue> + Copy + TypeOf,
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
    Y: Into<DynValue> + Copy + TypeOf,
{
    Box::new(Adapter::new(inner))
}

// ---------------------------------------------------------------------------
// chain: runtime-typed composition of two DynIndicators
// ---------------------------------------------------------------------------
// stable_gate: runtime-typed version of the library's Stable wrapper
// ---------------------------------------------------------------------------

/// Wrap `inner` in the library's [`Stable`](fugazi::indicators::Stable)
/// semantics at the runtime-typed layer: the returned `DynIndicator` reports
/// `warm_up = inner.stable_period()`, `unstable = 0`, and masks the inner
/// output until `stable_period` samples have elapsed. Same effect as
/// `IndicatorExt::stable`, but on the boxed side.
/// A `bool`-output [`DynIndicator`] that returns `Some(true)` from the
/// `stable_period`-th `update` onwards, mirroring the library-level
/// [`Stable`](fugazi::indicators::Stable). Doesn't hold any source — capture
/// the source's `stable_period()` at construction and drop it.
pub fn stable_check(stable_period: usize) -> Box<dyn DynIndicator> {
    Box::new(StableCheck {
        stable_period,
        samples: 0,
    })
}

struct StableCheck {
    stable_period: usize,
    samples: usize,
}

impl DynIndicator for StableCheck {
    fn input_type(&self) -> DynType {
        DynType::Candle
    }
    fn output_type(&self) -> DynType {
        DynType::Bool
    }
    fn update(&mut self, _x: DynValue) -> Option<DynValue> {
        self.samples = self.samples.saturating_add(1);
        Some(DynValue::Bool(self.samples >= self.stable_period))
    }
    fn value(&self) -> Option<DynValue> {
        Some(DynValue::Bool(self.samples >= self.stable_period))
    }
    fn warm_up_period(&self) -> usize {
        0
    }
    fn unstable_period(&self) -> usize {
        0
    }
    fn reset(&mut self) {
        self.samples = 0;
    }
    fn dyn_clone(&self) -> Box<dyn DynIndicator> {
        Box::new(StableCheck {
            stable_period: self.stable_period,
            samples: self.samples,
        })
    }
}

// ---------------------------------------------------------------------------
// Typed views: reconstitute a Box<dyn DynIndicator> as a library-typed
// Indicator<Input=Candle, Output=X> so it can drop into library constructors
// (Ema::new(source, period), IndicatorExt::gt(...), SingleAssetStrategy slots).
// ---------------------------------------------------------------------------

/// Views a `Box<dyn DynIndicator>` with `output_type == Real` as a library
/// `Indicator<Input = Candle, Output = Real>` — the shape every source-side
/// library constructor (Ema, Sma, arithmetic ops, comparisons, …) expects.
///
/// # Panics
/// [`new`](Self::new) panics if `inner.input_type() != Candle` or
/// `inner.output_type() != Real`; the recursive spec builder enforces both at
/// construction, so the unwrap arms in `update`/`value` are unreachable in
/// practice.
#[derive(Clone)]
pub struct AsReal(Box<dyn DynIndicator>);

impl AsReal {
    pub fn new(inner: Box<dyn DynIndicator>) -> Self {
        assert_eq!(
            inner.input_type(),
            DynType::Candle,
            "AsReal requires a Candle-input DynIndicator"
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
    type Input = Candle;
    type Output = Real;
    fn update(&mut self, c: Candle) -> Option<Real> {
        match self.0.update(DynValue::Candle(c))? {
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
/// `Indicator<Input = Candle, Output = bool>` — i.e. a
/// [`Signal`](fugazi::Signal).
///
/// # Panics
/// [`new`](Self::new) panics if `inner.input_type() != Candle` or
/// `inner.output_type() != Bool`.
#[derive(Clone)]
pub struct AsBool(Box<dyn DynIndicator>);

impl AsBool {
    pub fn new(inner: Box<dyn DynIndicator>) -> Self {
        assert_eq!(
            inner.input_type(),
            DynType::Candle,
            "AsBool requires a Candle-input DynIndicator"
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
    type Input = Candle;
    type Output = bool;
    fn update(&mut self, c: Candle) -> Option<bool> {
        match self.0.update(DynValue::Candle(c))? {
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
        assert_eq!(sma.input_type(), DynType::Candle);
        assert_eq!(sma.output_type(), DynType::Real);

        assert_eq!(sma.update(DynValue::Candle(bar(1.0))), None);
        assert_eq!(sma.update(DynValue::Candle(bar(2.0))), None);
        assert_eq!(
            sma.update(DynValue::Candle(bar(3.0))),
            Some(DynValue::Real(2.0))
        );
    }

    #[test]
    fn stable_check_reports_bool_after_threshold() {
        let raw = Ema::new(Current::close(), 3);
        let mut check = stable_check(raw.stable_period());
        assert_eq!(check.input_type(), DynType::Candle);
        assert_eq!(check.output_type(), DynType::Bool);
        // Feed stable_period - 1 candles; still Some(false).
        let bar = |v: Real| DynValue::Candle(Candle::new(v, v, v, v, 0.0));
        for i in 1..raw.stable_period() {
            assert_eq!(check.update(bar(i as Real)), Some(DynValue::Bool(false)));
        }
        // The stable_period-th update flips to Some(true).
        assert_eq!(
            check.update(bar(raw.stable_period() as Real)),
            Some(DynValue::Bool(true))
        );
    }

    #[test]
    fn stable_period_defaults_to_warm_up_plus_unstable() {
        let ema = wrap(Ema::new(Current::close(), 3));
        assert_eq!(
            ema.stable_period(),
            ema.warm_up_period() + ema.unstable_period()
        );
    }
}
