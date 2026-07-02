use crate::indicator::Indicator;
use crate::types::Real;

/// A pass-through source: yields each input unchanged.
///
/// Useful as a leaf in composition so a raw price stream can be compared
/// directly, e.g. `Gt::new(Identity::new(), Value::new(100.0))` expresses
/// "price above 100".
#[derive(Debug, Clone, Default)]
pub struct Identity {
    /// Latest input seen; `None` before the first update.
    pub value: Option<Real>,
}

impl Identity {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Indicator for Identity {
    type Input = Real;
    type Output = Real;

    fn update(&mut self, input: Real) -> Option<Real> {
        self.value = Some(input);
        self.value
    }

    fn value(&self) -> Option<Real> {
        self.value
    }

    fn warm_up_period(&self) -> usize {
        1
    }

    fn reset(&mut self) {
        self.value = None;
    }
}
