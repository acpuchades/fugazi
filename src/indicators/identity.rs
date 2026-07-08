use std::marker::PhantomData;

use crate::indicator::Indicator;
use crate::types::Real;

/// A pass-through source: yields each input unchanged.
///
/// Generic over the input type, defaulting to [`Real`] for the classic
/// raw-scalar case. `Identity::<Atom>::new()` is the atom passthrough used
/// as the default source for the candle- and calendar-input leaves
/// ([`Field`](super::Field), [`Calendar`](super::Calendar), …), so those
/// leaves can be re-rooted onto a different `Atom`-emitting source (a
/// cross-asset `!pick`, say) without changing their existing zero-arg
/// constructor.
#[derive(Debug, Clone)]
pub struct Identity<I = Real> {
    /// Latest input seen; `None` before the first update.
    pub value: Option<I>,
    _phantom: PhantomData<fn(I) -> I>,
}

impl<I> Identity<I> {
    pub fn new() -> Self {
        Self {
            value: None,
            _phantom: PhantomData,
        }
    }
}

impl<I> Default for Identity<I> {
    fn default() -> Self {
        Self::new()
    }
}

impl<I: Clone> Indicator for Identity<I> {
    type Input = I;
    type Output = I;

    fn update(&mut self, input: I) -> Option<I> {
        self.value = Some(input);
        self.value.clone()
    }

    fn value(&self) -> Option<I> {
        self.value.clone()
    }

    fn warm_up_period(&self) -> usize {
        1
    }

    fn reset(&mut self) {
        self.value = None;
    }
}
