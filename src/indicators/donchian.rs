use crate::indicator::Indicator;
use crate::indicators::{Component, RollingMax, RollingMin};
use crate::types::Real;

/// The three lines of a [`Donchian`] channel.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DonchianValue {
    /// Upper channel: highest high over the window.
    pub upper: Real,
    /// Middle channel: `(upper + lower) / 2`.
    pub middle: Real,
    /// Lower channel: lowest low over the window.
    pub lower: Real,
}

/// Donchian channel from a high source and a low source.
///
/// The upper line is the rolling maximum of the high source, the lower line the
/// rolling minimum of the low source, and the middle their midpoint. Classic
/// usage: `Donchian::new(Current::high(), Current::low(), 20)`. Both sources are
/// fed the same input each step (hence `Input: Clone`); produces `None` until
/// the window is full.
#[derive(Debug, Clone)]
pub struct Donchian<H, L> {
    high: RollingMax<H>,
    low: RollingMin<L>,
    /// Latest upper channel.
    pub upper: Option<Real>,
    /// Latest middle channel.
    pub middle: Option<Real>,
    /// Latest lower channel.
    pub lower: Option<Real>,
}

impl<H, L> Donchian<H, L> {
    /// # Panics
    /// Panics if `period` is zero.
    pub fn new(high: H, low: L, period: usize) -> Self {
        Self {
            high: RollingMax::new(high, period),
            low: RollingMin::new(low, period),
            upper: None,
            middle: None,
            lower: None,
        }
    }
}

/// Component accessors: each channel line as a standalone
/// `Indicator<Output = Real>`, so a line composes and compares like any other
/// source — e.g. `Current::close().crosses_above(channel.upper())`.
impl<H, L> Donchian<H, L>
where
    Donchian<H, L>: Indicator<Output = DonchianValue> + Clone,
{
    /// The upper channel (highest high over the window) as a standalone source.
    pub fn upper(&self) -> Component<Self> {
        Component::new(self.clone(), |v| v.upper)
    }

    /// The middle channel (`(upper + lower)/2`) as a standalone source.
    pub fn middle(&self) -> Component<Self> {
        Component::new(self.clone(), |v| v.middle)
    }

    /// The lower channel (lowest low over the window) as a standalone source.
    pub fn lower(&self) -> Component<Self> {
        Component::new(self.clone(), |v| v.lower)
    }
}

impl<H, L> Indicator for Donchian<H, L>
where
    H: Indicator<Output = Real>,
    L: Indicator<Input = H::Input, Output = Real>,
    H::Input: Clone,
{
    type Input = H::Input;
    type Output = DonchianValue;

    fn update(&mut self, input: Self::Input) -> Option<DonchianValue> {
        let upper = self.high.update(input.clone());
        let lower = self.low.update(input);

        match (upper, lower) {
            (Some(upper), Some(lower)) => {
                let middle = (upper + lower) / 2.0;
                self.upper = Some(upper);
                self.middle = Some(middle);
                self.lower = Some(lower);
                Some(DonchianValue {
                    upper,
                    middle,
                    lower,
                })
            }
            _ => {
                self.upper = None;
                self.middle = None;
                self.lower = None;
                None
            }
        }
    }

    fn value(&self) -> Option<DonchianValue> {
        match (self.upper, self.middle, self.lower) {
            (Some(upper), Some(middle), Some(lower)) => Some(DonchianValue {
                upper,
                middle,
                lower,
            }),
            _ => None,
        }
    }

    fn reset(&mut self) {
        self.high.reset();
        self.low.reset();
        self.upper = None;
        self.middle = None;
        self.lower = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indicators::Current;
    use crate::types::Candle;

    fn bar(high: Real, low: Real) -> Candle {
        Candle::new(low, high, low, low, 0.0)
    }

    #[test]
    fn tracks_window_high_and_low() {
        let mut dc = Donchian::new(Current::high(), Current::low(), 2);
        assert_eq!(dc.update(bar(10.0, 8.0)), None); // warming up
        let a = dc.update(bar(12.0, 9.0)).unwrap(); // highs [10,12], lows [8,9]
        assert_eq!(a.upper, 12.0);
        assert_eq!(a.lower, 8.0);
        assert_eq!(a.middle, 10.0);
        let b = dc.update(bar(11.0, 7.0)).unwrap(); // highs [12,11], lows [9,7]
        assert_eq!(b.upper, 12.0);
        assert_eq!(b.lower, 7.0);
    }
}
