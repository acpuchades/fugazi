use crate::indicator::Indicator;
use crate::indicators::smoothing::WilderState;
use crate::types::Real;

/// Relative Strength Index of a source (Wilder's smoothing).
///
/// Owns its input source: `Rsi::new(Current::close(), 14)`. Smooths up- and
/// down-moves of the source's output with two Wilder averages and forms
/// `RSI = 100 - 100 / (1 + avg_gain / avg_loss)`. Produces `None` until
/// `period + 1` source values have been seen.
#[derive(Debug, Clone)]
pub struct Rsi<S> {
    source: S,
    prev: Option<Real>,
    gain: WilderState,
    loss: WilderState,
    /// Latest RSI value in `[0, 100]`; `None` until warmed up.
    pub value: Option<Real>,
}

impl<S> Rsi<S> {
    /// # Panics
    /// Panics if `period` is zero.
    pub fn new(source: S, period: usize) -> Self {
        Self {
            source,
            prev: None,
            gain: WilderState::new(period),
            loss: WilderState::new(period),
            value: None,
        }
    }

    pub fn period(&self) -> usize {
        self.gain.period()
    }
}

impl<S: Indicator<Output = Real>> Indicator for Rsi<S> {
    type Input = S::Input;
    type Output = Real;

    fn update(&mut self, input: Self::Input) -> Option<Real> {
        let price = match self.source.update(input) {
            Some(x) => x,
            None => {
                self.value = None;
                return None;
            }
        };
        let prev = match self.prev {
            Some(prev) => prev,
            None => {
                // First source value: nothing to diff against yet.
                self.prev = Some(price);
                self.value = None;
                return None;
            }
        };
        self.prev = Some(price);

        let delta = price - prev;
        let avg_gain = self.gain.update(delta.max(0.0));
        let avg_loss = self.loss.update((-delta).max(0.0));

        self.value = match (avg_gain, avg_loss) {
            (Some(avg_gain), Some(avg_loss)) => Some(if avg_loss == 0.0 {
                100.0
            } else {
                let rs = avg_gain / avg_loss;
                100.0 - 100.0 / (1.0 + rs)
            }),
            _ => None,
        };
        self.value
    }

    fn value(&self) -> Option<Real> {
        self.value
    }

    fn warm_up_period(&self) -> usize {
        // One source output to seed the diff, then a full period of deltas.
        // `max(1)` so a `warm_up = 0` source still needs the seed update.
        self.source.warm_up_period().max(1) + self.gain.period()
    }

    fn unstable_period(&self) -> usize {
        // Both Wilder states share the period, so they settle together.
        self.source.unstable_period() + self.gain.unstable_period()
    }

    fn reset(&mut self) {
        self.source.reset();
        self.prev = None;
        self.gain.reset();
        self.loss.reset();
        self.value = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indicators::Identity;

    #[test]
    fn steadily_rising_prices_approach_100() {
        let mut rsi = Rsi::new(Identity::new(), 3);
        let mut last = None;
        for p in [1.0, 2.0, 3.0, 4.0, 5.0, 6.0] {
            last = rsi.update(p);
        }
        // Only gains -> avg_loss is zero -> RSI pinned at 100.
        assert_eq!(last, Some(100.0));
    }

    #[test]
    fn warms_up_after_period_plus_one() {
        let mut rsi = Rsi::new(Identity::new(), 2);
        assert_eq!(rsi.update(10.0), None); // seeds prev
        assert_eq!(rsi.update(11.0), None); // 1st delta
        assert!(rsi.update(12.0).is_some()); // 2nd delta -> ready
    }
}
