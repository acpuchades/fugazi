use crate::indicator::Indicator;
use crate::indicators::Component;
use crate::indicators::ops::{MaxOp, MinOp};
use crate::indicators::stats::WindowExtreme;
use crate::types::{Candle, Real};

/// The two lines and oscillator of [`Aroon`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AroonValue {
    /// Aroon Up: `100·(period − bars_since_highest_high)/period`, in `[0, 100]`.
    pub up: Real,
    /// Aroon Down: `100·(period − bars_since_lowest_low)/period`, in `[0, 100]`.
    pub down: Real,
    /// Aroon Oscillator: `up − down`, in `[-100, 100]`.
    pub oscillator: Real,
}

/// Aroon indicator (Chande): how recently the window's high and low occurred.
///
/// A bar indicator (consumes candles from an owned source). Each line measures
/// the bars elapsed since the extreme of the trailing `period + 1` bars: a
/// fresh high pins Aroon Up at `100`, decaying by `100/period` per bar until a
/// newer high appears (and symmetrically for Aroon Down on lows). Their
/// difference is the Aroon Oscillator. Both rolling extrema reuse the shared
/// [`WindowExtreme`] core via its `since` query. Ready after `period + 1` bars.
#[derive(Debug, Clone)]
pub struct Aroon<S> {
    source: S,
    period: usize,
    highest: WindowExtreme<MaxOp>,
    lowest: WindowExtreme<MinOp>,
    /// Latest Aroon Up.
    pub up: Option<Real>,
    /// Latest Aroon Down.
    pub down: Option<Real>,
    /// Latest Aroon Oscillator (`up − down`).
    pub oscillator: Option<Real>,
}

impl<S> Aroon<S> {
    /// # Panics
    /// Panics if `period` is zero.
    pub fn new(source: S, period: usize) -> Self {
        assert!(period > 0, "Aroon period must be greater than zero");
        Self {
            source,
            period,
            // The lookback spans `period + 1` bars (the current bar and the
            // `period` before it), so a brand-new extreme reads `since == 0`.
            highest: WindowExtreme::new(period + 1),
            lowest: WindowExtreme::new(period + 1),
            up: None,
            down: None,
            oscillator: None,
        }
    }

    pub fn period(&self) -> usize {
        self.period
    }
}

/// Component accessors: each output as a standalone `Indicator<Output = Real>`,
/// so e.g. `aroon.up().crosses_above(aroon.down())` or
/// `aroon.oscillator().above(0.0)`.
impl<S: Clone> Aroon<S>
where
    Aroon<S>: Indicator<Output = AroonValue>,
{
    /// Aroon Up as a standalone source.
    pub fn up(&self) -> Component<Self> {
        Component::new(self.clone(), |v| v.up)
    }

    /// Aroon Down as a standalone source.
    pub fn down(&self) -> Component<Self> {
        Component::new(self.clone(), |v| v.down)
    }

    /// The Aroon Oscillator (`up − down`) as a standalone source.
    pub fn oscillator(&self) -> Component<Self> {
        Component::new(self.clone(), |v| v.oscillator)
    }
}

impl<S: Indicator<Output = Candle>> Indicator for Aroon<S> {
    type Input = S::Input;
    type Output = AroonValue;

    fn update(&mut self, input: S::Input) -> Option<AroonValue> {
        let candle = self.source.update(input)?;
        self.highest.update(candle.high);
        self.lowest.update(candle.low);

        match (self.highest.since(), self.lowest.since()) {
            (Some(since_high), Some(since_low)) => {
                let p = self.period as Real;
                let up = 100.0 * (p - since_high as Real) / p;
                let down = 100.0 * (p - since_low as Real) / p;
                let oscillator = up - down;
                self.up = Some(up);
                self.down = Some(down);
                self.oscillator = Some(oscillator);
                Some(AroonValue {
                    up,
                    down,
                    oscillator,
                })
            }
            _ => {
                self.up = None;
                self.down = None;
                self.oscillator = None;
                None
            }
        }
    }

    fn value(&self) -> Option<AroonValue> {
        match (self.up, self.down, self.oscillator) {
            (Some(up), Some(down), Some(oscillator)) => Some(AroonValue {
                up,
                down,
                oscillator,
            }),
            _ => None,
        }
    }

    fn warm_up_period(&self) -> usize {
        // The lookback spans the current bar plus the `period` before it.
        self.source.warm_up_period().max(1) + self.period
    }

    fn unstable_period(&self) -> usize {
        self.source.unstable_period()
    }

    fn reset(&mut self) {
        self.source.reset();
        self.highest.reset();
        self.lowest.reset();
        self.up = None;
        self.down = None;
        self.oscillator = None;
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
    fn fresh_high_pins_up_and_decays() {
        let mut aroon = Aroon::new(Current::candle(), 3); // lookback of 4 bars
        assert_eq!(aroon.update(bar(5.0, 4.0).into()), None);
        assert_eq!(aroon.update(bar(6.0, 3.0).into()), None);
        assert_eq!(aroon.update(bar(7.0, 2.0).into()), None);
        // 4 bars seen: highest high is this bar (since 0) -> up 100; lowest low is
        // this bar too (since 0) -> down 100.
        let a = aroon.update(bar(8.0, 1.0).into()).unwrap();
        assert_eq!(a.up, 100.0);
        assert_eq!(a.down, 100.0);
        assert_eq!(a.oscillator, 0.0);
        // New bar with neither new high nor low: both extremes are now 1 bar old.
        // up = down = 100*(3-1)/3 = 66.67.
        let b = aroon.update(bar(7.5, 1.5).into()).unwrap();
        assert!((b.up - 200.0 / 3.0).abs() < 1e-12);
        assert!((b.down - 200.0 / 3.0).abs() < 1e-12);
    }
}
