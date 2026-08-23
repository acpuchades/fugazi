use fugazi_derive::SaveState;

use crate::indicator::Indicator;
use crate::indicators::stats::Ring;
use crate::types::{Candle, Real};

/// Volume-Weighted Average Price (VWAP), rolling over the last `period` bars.
///
/// A bar indicator (consumes candles from an owned source). Maintains running
/// sums of `typical * volume` and `volume` across the retained window, so each
/// update is O(1); the value is the ratio of the two. Typical price is
/// `(high + low + close) / 3` (see [`Candle::typical`]).
///
/// Anchored / session VWAP is not modelled — the crate has no notion of
/// trading sessions, so the rolling form is the only shape that generalises
/// across the 24/7 markets it targets. Ready once `period` bars have been
/// observed *and* the retained window carries non-zero volume (a stretch of
/// zero-volume bars in the window returns `None`).
#[derive(Debug, Clone, SaveState)]
pub struct Vwap<S> {
    #[state(source)]
    source: S,
    #[state(config)]
    period: usize,
    #[state(window)]
    window: Ring<(Real, Real)>,
    sum_pv: Real,
    sum_volume: Real,
    value: Option<Real>,
}

impl<S> Vwap<S> {
    pub fn new(source: S, period: usize) -> Self {
        assert!(period > 0, "VWAP period must be greater than zero");
        Self {
            source,
            period,
            window: Ring::new(period),
            sum_pv: 0.0,
            sum_volume: 0.0,
            value: None,
        }
    }
}

impl<S: Indicator<Output = Candle>> Indicator for Vwap<S> {
    type Input = S::Input;
    type Output = Real;

    fn update(&mut self, input: S::Input) -> Option<Real> {
        let candle = self.source.update(input)?;
        let pv = candle.typical() * candle.volume;
        // `Ring::push` evicts and returns the oldest in one operation; the
        // arithmetic order is unchanged from the `push_back` / conditional
        // `pop_front` pair it replaces, so the value stays bit-identical.
        let evicted = self.window.push((pv, candle.volume));
        self.sum_pv += pv;
        self.sum_volume += candle.volume;
        if let Some((old_pv, old_v)) = evicted {
            self.sum_pv -= old_pv;
            self.sum_volume -= old_v;
        }
        self.value = (self.window.is_full() && self.sum_volume != 0.0)
            .then(|| self.sum_pv / self.sum_volume);
        self.value
    }

    fn value(&self) -> Option<Real> {
        self.value
    }

    fn warm_up_bars(&self) -> usize {
        self.source.warm_up_bars() + self.period - 1
    }

    fn unstable_bars(&self) -> usize {
        self.source.unstable_bars()
    }

    fn reset(&mut self) {
        self.source.reset();
        self.window.clear();
        self.sum_pv = 0.0;
        self.sum_volume = 0.0;
        self.value = None;
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
    use crate::types::{Atom, Candle};

    #[test]
    fn weights_price_by_volume_over_window() {
        let mut vwap = Vwap::new(Current::candle(), 2);
        // First bar: window not full yet.
        assert_eq!(
            vwap.update(Candle::new(10.0, 10.0, 10.0, 10.0, 100.0).into()),
            None
        );
        // Second bar completes the window: (10*100 + 20*300) / 400 = 17.5
        assert_eq!(
            vwap.update(Candle::new(20.0, 20.0, 20.0, 20.0, 300.0).into()),
            Some(17.5)
        );
        // Third bar evicts the first: (20*300 + 30*200) / 500 = 24.0
        assert_eq!(
            vwap.update(Candle::new(30.0, 30.0, 30.0, 30.0, 200.0).into()),
            Some(24.0)
        );
    }

    /// `Vwap`'s window moved from a `VecDeque<(Real, Real)>` to a
    /// `Ring<(Real, Real)>`. A literal blob in the old derive's shape — `window`
    /// as a bare oldest-first array of `[pv, volume]` pairs — must still resume
    /// a run identically.
    #[test]
    fn the_pre_ring_vwap_state_still_resumes() {
        use crate::Indicator as _;

        let build = || Vwap::new(Current::candle(), 2);
        let mut paused = build();
        let blob = serde_json::json!({
            "source": build().source.save_state(),
            "period": 2,
            // One bar seen: typical 10.0 * volume 100.0 = 1000.0.
            "window": [[1000.0, 100.0]],
            "sum_pv": 1000.0,
            "sum_volume": 100.0,
            "value": null,
        });
        paused.load_state(&blob).expect("legacy vwap state");

        let mut twin = build();
        twin.update(Candle::new(10.0, 10.0, 10.0, 10.0, 100.0).into());

        // Same arithmetic as `weights_price_by_volume_over_window`, so the
        // window really was restored at one sample rather than at capacity.
        let second: Atom = Candle::new(20.0, 20.0, 20.0, 20.0, 300.0).into();
        assert_eq!(paused.update(second.clone()), twin.update(second));
        assert_eq!(paused.value(), Some(17.5));

        let third: Atom = Candle::new(30.0, 30.0, 30.0, 30.0, 200.0).into();
        assert_eq!(paused.update(third.clone()), twin.update(third));
        assert_eq!(paused.value(), Some(24.0));

        // And the shape it writes back is the one it just read.
        assert_eq!(
            paused.save_state()["window"],
            serde_json::json!([[6000.0, 300.0], [6000.0, 200.0]]),
            "window is no longer a bare oldest-first array"
        );
    }

    #[test]
    fn zero_volume_window_is_not_ready() {
        let mut vwap = Vwap::new(Current::candle(), 2);
        assert_eq!(
            vwap.update(Candle::new(10.0, 10.0, 10.0, 10.0, 0.0).into()),
            None
        );
        assert_eq!(
            vwap.update(Candle::new(20.0, 20.0, 20.0, 20.0, 0.0).into()),
            None
        );
    }
}
