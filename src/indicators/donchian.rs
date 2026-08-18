use fugazi_derive::SaveState;

use crate::indicator::Indicator;
use crate::indicators::{RollingMax, RollingMin};
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
#[derive(Debug, Clone, SaveState)]
pub struct Donchian<H, L> {
    #[state(source)]
    high: RollingMax<H>,
    #[state(source)]
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

// Component accessors: each channel line as a standalone
// `Indicator<Output = Real>`, so a line composes and compares like any other
// source — e.g. `Current::close().crosses_above(channel.upper())`.
crate::indicators::component::component_accessors!(
    Donchian<H, L>, DonchianValue;
    /// The upper channel (highest high over the window) as a standalone source.
    upper => upper,
    /// The middle channel (`(upper + lower)/2`) as a standalone source.
    middle => middle,
    /// The lower channel (lowest low over the window) as a standalone source.
    lower => lower,
);

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

    fn warm_up_bars(&self) -> usize {
        self.high.warm_up_bars().max(self.low.warm_up_bars())
    }

    fn unstable_bars(&self) -> usize {
        self.high.stable_bars().max(self.low.stable_bars()) - self.warm_up_bars()
    }

    fn reset(&mut self) {
        self.high.reset();
        self.low.reset();
        self.upper = None;
        self.middle = None;
        self.lower = None;
    }

    fn save_state(&self) -> serde_json::Value {
        self.save_state_fields()
    }

    fn load_state(&mut self, state: &serde_json::Value) -> Result<(), String> {
        self.load_state_fields(state)
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
        assert_eq!(dc.update(bar(10.0, 8.0).into()), None); // warming up
        let a = dc.update(bar(12.0, 9.0).into()).unwrap(); // highs [10,12], lows [8,9]
        assert_eq!(a.upper, 12.0);
        assert_eq!(a.lower, 8.0);
        assert_eq!(a.middle, 10.0);
        let b = dc.update(bar(11.0, 7.0).into()).unwrap(); // highs [12,11], lows [9,7]
        assert_eq!(b.upper, 12.0);
        assert_eq!(b.lower, 7.0);
    }

    /// The channel's window **ends on the bar being evaluated**, so `close`
    /// is inside it by construction: `close <= high <= upper` and
    /// `close >= low >= lower`, on every settled bar. That makes the textbook
    /// breakout — `close` crossing above `!donchian_upper` — a guaranteed
    /// no-op, which is a real trap (it builds, runs, and reports zero trades)
    /// and is why `docs/STRATEGIES.md` documents the `!lag` form next to the
    /// tag.
    ///
    /// Pinned here because it is the *premise* of that documentation: if the
    /// window is ever changed to exclude the current bar, this fails and the
    /// docs need rewriting rather than silently becoming wrong.
    #[test]
    fn the_channel_always_contains_the_current_bar() {
        let mut dc = Donchian::new(Current::high(), Current::low(), 5);
        // A rising sawtooth, so new highs are frequent and each one is the
        // bar most likely to escape its own channel.
        for i in 0..60u32 {
            let mid = 100.0 + Real::from(i) * 0.7 + Real::from(i % 7) * 2.0;
            let (high, low) = (mid + 1.0, mid - 1.0);
            let candle = bar(high, low);
            let close = candle.close;
            if let Some(v) = dc.update(candle.into()) {
                assert!(
                    close <= v.upper,
                    "bar {i}: close {close} escaped upper {}",
                    v.upper
                );
                assert!(
                    close >= v.lower,
                    "bar {i}: close {close} escaped lower {}",
                    v.lower
                );
                assert!(high <= v.upper, "bar {i}: high above its own channel");
                assert!(low >= v.lower, "bar {i}: low below its own channel");
            }
        }
    }

}
