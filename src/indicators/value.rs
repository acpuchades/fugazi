use std::marker::PhantomData;
use std::sync::Arc;

use crate::indicator::Indicator;
use crate::types::Real;

/// A constant source indicator: always yields the same value, ignoring input.
///
/// This is the leaf that lets a literal participate in indicator/signal
/// composition, e.g. `Gt::new(Rsi::new(14), Value::new(70.0))`.
///
/// Generic over the input type so it can share an `Input` with whatever it is
/// composed against (the input is ignored).
#[derive(Debug, Clone, Copy)]
pub struct Value<I> {
    constant: Real,
    _input: PhantomData<fn(I)>,
}

impl<I> Value<I> {
    pub fn new(constant: Real) -> Self {
        Self {
            constant,
            _input: PhantomData,
        }
    }

    pub fn constant(&self) -> Real {
        self.constant
    }
}

impl<I> Indicator for Value<I> {
    type Input = I;
    type Output = Real;

    fn update(&mut self, _input: I) -> Option<Real> {
        Some(self.constant)
    }

    fn value(&self) -> Option<Real> {
        Some(self.constant)
    }

    fn warm_up_period(&self) -> usize {
        0
    }

    fn reset(&mut self) {}
}

/// The string twin of [`Value`]: a constant `Arc<str>` source that ignores its
/// input.
///
/// Lets a string literal take part in string-typed composition — e.g. the
/// right-hand side of a [`StrEq`](super::compare::StrEq) against a
/// [`GetStr`](super::GetStr) column read.
#[derive(Debug, Clone)]
pub struct ValueStr<I> {
    constant: Arc<str>,
    _input: PhantomData<fn(I)>,
}

impl<I> ValueStr<I> {
    pub fn new(constant: impl Into<Arc<str>>) -> Self {
        Self {
            constant: constant.into(),
            _input: PhantomData,
        }
    }

    pub fn constant(&self) -> &Arc<str> {
        &self.constant
    }
}

impl<I> Indicator for ValueStr<I> {
    type Input = I;
    type Output = Arc<str>;

    fn update(&mut self, _input: I) -> Option<Arc<str>> {
        Some(self.constant.clone())
    }

    fn value(&self) -> Option<Arc<str>> {
        Some(self.constant.clone())
    }

    fn warm_up_period(&self) -> usize {
        0
    }

    fn reset(&mut self) {}
}
