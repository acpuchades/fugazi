//! Type-erased indicator carriers.
//!
//! The core `fugazi` indicators are statically composed — `Ema<S>`, `Gt<L, R>`,
//! `And<L, R>` and so on are distinct types parameterised by their sources. A
//! YAML-driven builder, by contrast, only learns the shape of the tree at
//! runtime, so it needs a *single* erased type it can return from every match
//! arm and then nest into the next constructor.
//!
//! [`DynValue`] and [`DynSignal`] are those types: thin newtypes wrapping a
//! boxed `Indicator`, each re-implementing [`Indicator`] by forwarding to the
//! inner box. Because they are **local** types, implementing the foreign
//! `Indicator` trait on them is orphan-rule-safe — the core crate needs no
//! changes. The names mirror the crate's two indicator roles: a `DynValue` is an
//! erased real-valued source (`Output = Real`), a `DynSignal` an erased *signal*
//! (`Output = bool`).
//!
//! Two properties make them drop-in operands for the existing carriers:
//!
//! * `DynValue: Indicator<Input = Candle, Output = Real>`, so it satisfies the
//!   `S: Indicator<Output = Real>` bound on `Ema::new`, the `Combine` operand
//!   bounds, etc. (`Candle: Clone` covers `Combine`'s `Input: Clone`).
//! * `DynSignal: Indicator<Input = Candle, Output = bool>`, which is exactly the
//!   definition of a [`Signal`](fugazi::Signal) (blanket-implemented), so a
//!   `DynSignal` drops straight into `SingleAssetStrategy::long_on`/`short_on`,
//!   whose slots take `impl Signal + 'static`.

use fugazi::prelude::*;

/// A real-valued source that can also clone itself behind a box.
///
/// Some indicators clone their source internally (e.g. `Hma`, which is a WMA of
/// WMAs, and the multi-output component accessors), so they bound `S: Clone`.
/// Erasing through a plain `Box<dyn Indicator>` would lose `Clone`; this
/// supertrait threads it back, so [`DynValue`] is itself `Clone` and composes
/// into those indicators like any concrete source.
trait CloneableValue: Indicator<Input = Candle, Output = Real> {
    fn clone_box(&self) -> Box<dyn CloneableValue>;
}

impl<T> CloneableValue for T
where
    T: Indicator<Input = Candle, Output = Real> + Clone + 'static,
{
    fn clone_box(&self) -> Box<dyn CloneableValue> {
        Box::new(self.clone())
    }
}

/// A type-erased real-valued source over a [`Candle`] stream.
pub struct DynValue(Box<dyn CloneableValue>);

impl DynValue {
    /// Box `source` into a `DynValue`.
    pub fn new(source: impl Indicator<Input = Candle, Output = Real> + Clone + 'static) -> Self {
        DynValue(Box::new(source))
    }
}

impl Clone for DynValue {
    fn clone(&self) -> Self {
        DynValue(self.0.clone_box())
    }
}

impl Indicator for DynValue {
    type Input = Candle;
    type Output = Real;

    fn update(&mut self, candle: Candle) -> Option<Real> {
        self.0.update(candle)
    }

    fn value(&self) -> Option<Real> {
        self.0.value()
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

/// A type-erased boolean condition over a [`Candle`] stream — i.e. a
/// [`Signal`](fugazi::Signal).
pub struct DynSignal(pub Box<dyn Indicator<Input = Candle, Output = bool>>);

impl DynSignal {
    /// Box `signal` into a `DynSignal`.
    pub fn new(signal: impl Indicator<Input = Candle, Output = bool> + 'static) -> Self {
        DynSignal(Box::new(signal))
    }
}

impl Indicator for DynSignal {
    type Input = Candle;
    type Output = bool;

    fn update(&mut self, candle: Candle) -> Option<bool> {
        self.0.update(candle)
    }

    fn value(&self) -> Option<bool> {
        self.0.value()
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
