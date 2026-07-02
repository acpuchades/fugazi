use crate::indicator::Indicator;
use crate::types::{Candle, Real};

/// TA-Lib's default acceleration step and cap.
const DEFAULT_STEP: Real = 0.02;
const DEFAULT_MAX: Real = 0.2;

/// Parabolic SAR (Wilder's stop-and-reverse).
///
/// A bar indicator (consumes the full [`Candle`]) and a genuinely recursive one:
/// it trails price by a stop that accelerates toward the running extreme point
/// (`EP`) of the current trend. Each step the stop moves
/// `SAR += AF·(EP − SAR)`; the acceleration factor `AF` ramps by `step` (capped
/// at `max`) every time the trend posts a new extreme. When price crosses the
/// stop the position flips: the stop jumps to the prior `EP`, `AF` resets, and a
/// fresh trend begins.
///
/// Matches TA-Lib's `SAR`: the initial trend direction is taken from the first
/// two bars' directional movement, the stop is clamped within the prior two
/// bars' range so it never lands inside them, and the first value is produced on
/// the **second** bar.
#[derive(Debug, Clone)]
pub struct Sar {
    step: Real,
    max: Real,
    bars: usize,
    is_long: bool,
    af: Real,
    /// Extreme point of the current trend.
    ep: Real,
    /// SAR to apply on the next bar.
    sar: Real,
    /// Previous bar's high and low.
    prev_high: Real,
    prev_low: Real,
    /// Latest SAR value; `None` until the second bar.
    pub value: Option<Real>,
}

impl Sar {
    /// Construct with an explicit acceleration `step` and `max` cap (TA-Lib's
    /// defaults are `0.02` and `0.2` — see [`Sar::default`]).
    ///
    /// # Panics
    /// Panics unless `0 < step <= max`.
    pub fn new(step: Real, max: Real) -> Self {
        assert!(step > 0.0, "SAR acceleration step must be positive");
        assert!(max >= step, "SAR maximum must be at least the step");
        Self {
            step,
            max,
            bars: 0,
            is_long: true,
            af: step,
            ep: 0.0,
            sar: 0.0,
            prev_high: 0.0,
            prev_low: 0.0,
            value: None,
        }
    }

    /// SAR to apply on the next bar: accelerate toward the extreme, then clamp so
    /// it stays beyond the prior two bars' range (below for a long, above for a
    /// short).
    fn next_sar(&self, base: Real, high: Real, low: Real) -> Real {
        let sar = base + self.af * (self.ep - base);
        if self.is_long {
            sar.min(self.prev_low).min(low)
        } else {
            sar.max(self.prev_high).max(high)
        }
    }
}

impl Default for Sar {
    fn default() -> Self {
        Self::new(DEFAULT_STEP, DEFAULT_MAX)
    }
}

impl Indicator for Sar {
    type Input = Candle;
    type Output = Real;

    fn update(&mut self, candle: Candle) -> Option<Real> {
        let (high, low) = (candle.high, candle.low);

        let out = match self.bars {
            0 => {
                // Need a prior bar before any directional movement exists.
                self.prev_high = high;
                self.prev_low = low;
                self.bars = 1;
                return None;
            }
            1 => {
                // Seed the trend from the first two bars' directional movement.
                let up_move = high - self.prev_high;
                let down_move = self.prev_low - low;
                self.is_long = !(down_move > up_move && down_move > 0.0);
                self.af = self.step;
                // First stop sits at the prior bar's extreme; EP is this bar's.
                let out = if self.is_long {
                    self.ep = high;
                    self.prev_low
                } else {
                    self.ep = low;
                    self.prev_high
                };
                // TA-Lib does not range-clamp this very first recurrence; the
                // clamp only kicks in from the next bar onward.
                self.sar = out + self.af * (self.ep - out);
                self.bars = 2;
                out
            }
            _ => {
                let reversed = if self.is_long {
                    low <= self.sar
                } else {
                    high >= self.sar
                };
                if reversed {
                    // Flip: the stop jumps to the old trend's extreme point.
                    self.is_long = !self.is_long;
                    let mut out = self.ep;
                    out = if self.is_long {
                        out.min(self.prev_low).min(low)
                    } else {
                        out.max(self.prev_high).max(high)
                    };
                    self.af = self.step;
                    self.ep = if self.is_long { high } else { low };
                    self.sar = self.next_sar(out, high, low);
                    out
                } else {
                    let out = self.sar;
                    // Extend the trend's extreme, accelerating when it advances.
                    if self.is_long {
                        if high > self.ep {
                            self.ep = high;
                            self.af = (self.af + self.step).min(self.max);
                        }
                    } else if low < self.ep {
                        self.ep = low;
                        self.af = (self.af + self.step).min(self.max);
                    }
                    self.sar = self.next_sar(out, high, low);
                    out
                }
            }
        };

        self.prev_high = high;
        self.prev_low = low;
        self.value = Some(out);
        self.value
    }

    fn value(&self) -> Option<Real> {
        self.value
    }

    /// `2` — the first bar only seeds the trend direction. Following TA-Lib,
    /// SAR reports no unstable period: each stop-and-reverse re-anchors the
    /// recursion, so the seed's influence ends at the first reversal rather
    /// than decaying gradually.
    fn warm_up_period(&self) -> usize {
        2
    }

    fn reset(&mut self) {
        self.bars = 0;
        self.is_long = true;
        self.af = self.step;
        self.ep = 0.0;
        self.sar = 0.0;
        self.prev_high = 0.0;
        self.prev_low = 0.0;
        self.value = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar(high: Real, low: Real) -> Candle {
        Candle::new(low, high, low, low, 0.0)
    }

    #[test]
    fn matches_reference_sequence() {
        // Verified against TA-Lib's SAR(high, low, 0.02, 0.2) for this series.
        let highs = [10.0, 11.0, 12.0, 11.0, 10.0, 9.0, 10.0, 11.0, 12.0, 13.0];
        let expected = [
            None,
            Some(9.0),
            Some(9.04),
            Some(9.1584),
            Some(12.0),
            Some(11.94),
            Some(11.7824),
            Some(11.631104),
            Some(8.0),
            Some(8.08),
        ];
        let mut sar = Sar::default();
        for (h, exp) in highs.iter().zip(expected) {
            let got = sar.update(bar(*h, h - 1.0));
            match (got, exp) {
                (Some(g), Some(e)) => assert!((g - e).abs() < 1e-6, "got {g}, expected {e}"),
                (None, None) => {}
                _ => panic!("readiness mismatch: got {got:?}, expected {exp:?}"),
            }
        }
    }
}
