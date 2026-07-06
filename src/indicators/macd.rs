use crate::indicator::Indicator;
use crate::indicators::smoothing::EmaState;
use crate::types::Real;

/// The three outputs of [`Macd`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MacdValue {
    /// The MACD line: `fast EMA - slow EMA`.
    pub macd: Real,
    /// The signal line: EMA of the MACD line.
    pub signal: Real,
    /// The histogram: `macd - signal`.
    pub histogram: Real,
}

/// Moving Average Convergence/Divergence of a source.
///
/// Owns its input source: `Macd::new(Current::close(), 12, 26, 9)`. The fast and
/// slow EMAs of the source form the MACD line, smoothed by the signal EMA.
/// Because the EMAs seed on their first sample, all three outputs are available
/// as soon as the source is; the values simply stabilise with more data.
///
/// Individual outputs are exposed as public fields and refreshed every update.
#[derive(Debug, Clone)]
pub struct Macd<S> {
    source: S,
    fast: EmaState,
    slow: EmaState,
    signal_ema: EmaState,
    /// Latest MACD line value.
    pub macd: Option<Real>,
    /// Latest signal line value.
    pub signal: Option<Real>,
    /// Latest histogram value.
    pub histogram: Option<Real>,
}

impl<S> Macd<S> {
    /// # Panics
    /// Panics if any period is zero.
    pub fn new(source: S, fast_period: usize, slow_period: usize, signal_period: usize) -> Self {
        Self {
            source,
            fast: EmaState::new(fast_period),
            slow: EmaState::new(slow_period),
            signal_ema: EmaState::new(signal_period),
            macd: None,
            signal: None,
            histogram: None,
        }
    }
}

// Component accessors: each yields one output line as a standalone
// `Indicator<Output = Real>`, so MACD's components compose and compare like any
// other source — e.g. `macd.line().crosses_above(macd.signal())`.
crate::indicators::component::component_accessors!(
    Macd<S>, MacdValue;
    /// The MACD line (fast EMA − slow EMA) as a standalone source.
    line => macd,
    /// The signal line (EMA of the MACD line) as a standalone source.
    signal => signal,
    /// The histogram (MACD line − signal line) as a standalone source.
    histogram => histogram,
);

impl<S: Indicator<Output = Real>> Indicator for Macd<S> {
    type Input = S::Input;
    type Output = MacdValue;

    fn update(&mut self, input: Self::Input) -> Option<MacdValue> {
        let price = match self.source.update(input) {
            Some(x) => x,
            None => {
                self.macd = None;
                self.signal = None;
                self.histogram = None;
                return None;
            }
        };

        // Both EMAs are ready from their first sample.
        let fast = self.fast.update(price).expect("EMA ready after update");
        let slow = self.slow.update(price).expect("EMA ready after update");
        let macd = fast - slow;
        let signal = self
            .signal_ema
            .update(macd)
            .expect("EMA ready after update");
        let histogram = macd - signal;

        self.macd = Some(macd);
        self.signal = Some(signal);
        self.histogram = Some(histogram);

        Some(MacdValue {
            macd,
            signal,
            histogram,
        })
    }

    fn value(&self) -> Option<MacdValue> {
        match (self.macd, self.signal, self.histogram) {
            (Some(macd), Some(signal), Some(histogram)) => Some(MacdValue {
                macd,
                signal,
                histogram,
            }),
            _ => None,
        }
    }

    fn warm_up_period(&self) -> usize {
        // All three EMAs seed on their first sample, so the whole triple is
        // ready as soon as the source is. `max(1)` because seeding needs at
        // least one `update` call.
        self.source.warm_up_period().max(1)
    }

    fn unstable_period(&self) -> usize {
        // The MACD line settles once the slower-settling of the two price EMAs
        // has; the signal EMA then re-smooths it, adding its own settling.
        self.source.unstable_period()
            + self.fast.unstable_period().max(self.slow.unstable_period())
            + self.signal_ema.unstable_period()
    }

    fn reset(&mut self) {
        self.source.reset();
        self.fast.reset();
        self.slow.reset();
        self.signal_ema.reset();
        self.macd = None;
        self.signal = None;
        self.histogram = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indicators::Identity;

    #[test]
    fn histogram_is_macd_minus_signal() {
        let mut macd = Macd::new(Identity::new(), 3, 6, 4);
        let mut last = None;
        for p in [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0] {
            last = macd.update(p);
        }
        let out = last.unwrap();
        assert!((out.histogram - (out.macd - out.signal)).abs() < 1e-12);
    }
}
