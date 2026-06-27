use std::marker::PhantomData;

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

    fn reset(&mut self) {}
}
