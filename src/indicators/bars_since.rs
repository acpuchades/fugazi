//! [`BarsSince`]: how many bars have elapsed since a boolean source last read
//! `true`, plus the two rolling-extremum shorthands [`BarsSinceHigh`] and
//! [`BarsSinceLow`].
//!
//! The event-recency primitive. `!rolling_max` tracks *what* the window's
//! extremum is; these track *when* it happened — the missing half. Composes
//! into freshness filters (`bars_since(cross) < 5` — only act on a signal
//! that fired recently), time-stops (`bars_since(entry_condition) > 20`), and
//! the Aroon identity (`100·(period − bars_since_high)/period`).

use fugazi_derive::SaveState;

use crate::indicator::Indicator;
use crate::indicators::ops::{MaxOp, MinOp};
use crate::indicators::stats::WindowExtreme;
use crate::types::Real;

/// Bars elapsed since `source` last read `true` — `0` on the firing bar itself,
/// `1` on the bar after, and so on.
///
/// # The never-fired case
///
/// Emits `None` until `source` has read `true` at least once. This follows the
/// crate's `Option` discipline (an unobserved quantity is `None`, not a
/// sentinel), and it is the conservative reading in *both* directions, because
/// `None` makes any downstream comparison read false:
///
/// * `bars_since(x).lt(5.0)` — "did `x` fire recently?" — is false while `x`
///   has never fired, so a never-fired signal cannot gate an entry *in*.
/// * `bars_since(x).gt(20.0)` — "has it been a long time since `x`?" — is
///   likewise false, so a clock that never started cannot time-stop a
///   position *out*.
///
/// A `None` tick from `source` (a source that goes unsettled mid-stream, e.g.
/// [`Log`](super::Log) on a non-positive input) emits `None` for that bar
/// without advancing the counter: no observation, no elapsed bar.
///
/// # Warm-up
///
/// [`warm_up_period`](Indicator::warm_up_period) reports the source's own
/// warm-up, which is a **lower bound** on the first `Some` rather than the
/// exact position — the true first `Some` lands on the source's first `true`,
/// which is data-dependent and unbounded. Like [`IfElse`](super::IfElse) and
/// the trailing risk indicators, this indicator is therefore excluded from the
/// `tests/warm_up.rs` exact-warm-up battery and covered by unit tests here.
///
/// # Example
///
/// ```
/// use fugazi::prelude::*;
/// use fugazi::indicators::{BarsSince, Identity};
///
/// // Fires whenever the input exceeds 10.
/// let mut bars = BarsSince::new(Identity::new().above(10.0));
/// assert_eq!(bars.update(1.0), None);        // never fired yet
/// assert_eq!(bars.update(42.0), Some(0.0));  // fires now
/// assert_eq!(bars.update(1.0), Some(1.0));   // one bar since
/// assert_eq!(bars.update(1.0), Some(2.0));
/// assert_eq!(bars.update(99.0), Some(0.0));  // fires again, counter resets
/// ```
#[derive(Debug, Clone, SaveState)]
pub struct BarsSince<S> {
    #[state(source)]
    source: S,
    /// `None` until the source has fired once; `Some(n)` thereafter.
    count: Option<usize>,
    /// Latest elapsed-bar count; `None` until the source has fired once.
    pub value: Option<Real>,
}

impl<S> BarsSince<S> {
    /// Build a counter over `source`.
    pub fn new(source: S) -> Self {
        Self {
            source,
            count: None,
            value: None,
        }
    }
}

impl<S: Indicator<Output = bool>> Indicator for BarsSince<S> {
    type Input = S::Input;
    type Output = Real;

    fn update(&mut self, input: Self::Input) -> Option<Real> {
        match self.source.update(input) {
            Some(true) => self.count = Some(0),
            // Only advance the counter once it has started: a `false` before
            // the first `true` leaves the count unset (never fired).
            Some(false) => self.count = self.count.map(|n| n.saturating_add(1)),
            // Unsettled source: no observation this bar, so no elapsed bar.
            None => {}
        }
        self.value = self.count.map(|n| n as Real);
        self.value
    }

    fn value(&self) -> Option<Real> {
        self.value
    }

    fn warm_up_period(&self) -> usize {
        // A lower bound — see the type-level docs. The earliest possible first
        // `Some` is the bar the source itself first reads `Some(true)`.
        self.source.warm_up_period()
    }

    fn unstable_period(&self) -> usize {
        self.source.unstable_period()
    }

    fn reset(&mut self) {
        self.source.reset();
        // Back to never-fired, not to zero: zero would falsely claim the
        // source just fired on the reset bar.
        self.count = None;
        self.value = None;
    }

    fn save_state(&self) -> serde_json::Value {
        self.save_state_fields()
    }

    fn load_state(&mut self, state: &serde_json::Value) -> Result<(), String> {
        self.load_state_fields(state)
    }
}

/// Bars elapsed since `source` last set a new `period`-bar high — `0` on the
/// bar that sets it, up to `period - 1` for a high about to leave the window.
///
/// The O(1) shorthand for `BarsSince::new(source.ge(source.rolling_max(period)))`,
/// backed by the shared [`WindowExtreme`] core's `since` query — the same core
/// (and the same query) [`Aroon`](super::Aroon) is built on. Unlike the general
/// [`BarsSince`], the window's high is always attained *somewhere* in the
/// window, so the first `Some` lands exactly on a full window and the warm-up
/// is exact.
///
/// On ties the most recent occurrence wins, so a flat series reads `0` forever.
///
/// # Example
///
/// ```
/// use fugazi::prelude::*;
/// use fugazi::indicators::{BarsSinceHigh, Identity};
///
/// let mut bars = BarsSinceHigh::new(Identity::new(), 3);
/// assert_eq!(bars.update(5.0), None);       // window not full
/// assert_eq!(bars.update(3.0), None);
/// assert_eq!(bars.update(1.0), Some(2.0));  // the high (5.0) was 2 bars ago
/// assert_eq!(bars.update(9.0), Some(0.0));  // new high, now
/// ```
#[derive(Debug, Clone, SaveState)]
pub struct BarsSinceHigh<S> {
    #[state(source)]
    source: S,
    extreme: WindowExtreme<MaxOp>,
    /// Latest bars-since-high; `None` until the window is full.
    pub value: Option<Real>,
}

/// Bars elapsed since `source` last set a new `period`-bar low.
///
/// The low-side twin of [`BarsSinceHigh`]; see there for the full contract.
#[derive(Debug, Clone, SaveState)]
pub struct BarsSinceLow<S> {
    #[state(source)]
    source: S,
    extreme: WindowExtreme<MinOp>,
    /// Latest bars-since-low; `None` until the window is full.
    pub value: Option<Real>,
}

// The two extremum shorthands differ only in the `ExtremeOp` marker, so their
// bodies are generated rather than written twice.
macro_rules! bars_since_extreme {
    ($ty:ident, $op:ident, $what:literal) => {
        impl<S> $ty<S> {
            #[doc = concat!("Build a bars-since-", $what, " counter over the last `period` samples.")]
            ///
            /// # Panics
            /// Panics if `period` is zero.
            pub fn new(source: S, period: usize) -> Self {
                Self {
                    source,
                    extreme: WindowExtreme::new(period),
                    value: None,
                }
            }

            pub fn period(&self) -> usize {
                self.extreme.period()
            }
        }

        impl<S: Indicator<Output = Real>> Indicator for $ty<S> {
            type Input = S::Input;
            type Output = Real;

            fn update(&mut self, input: Self::Input) -> Option<Real> {
                self.value = match self.source.update(input) {
                    Some(x) => {
                        self.extreme.update(x);
                        self.extreme.since().map(|n| n as Real)
                    }
                    None => None,
                };
                self.value
            }

            fn value(&self) -> Option<Real> {
                self.value
            }

            fn warm_up_period(&self) -> usize {
                self.source.warm_up_period().max(1) + self.extreme.period() - 1
            }

            fn unstable_period(&self) -> usize {
                self.source.unstable_period()
            }

            fn reset(&mut self) {
                self.source.reset();
                self.extreme.reset();
                self.value = None;
            }

            fn save_state(&self) -> serde_json::Value {
                self.save_state_fields()
            }

            fn load_state(&mut self, state: &serde_json::Value) -> Result<(), String> {
                self.load_state_fields(state)
            }
        }
    };
}

bars_since_extreme!(BarsSinceHigh, MaxOp, "high");
bars_since_extreme!(BarsSinceLow, MinOp, "low");

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indicators::ext::{BoolIndicatorExt, IndicatorExt};
    use crate::indicators::{ValueBool, Identity};

    /// A signal that fires whenever the input exceeds 10.
    fn spike() -> impl Indicator<Input = Real, Output = bool> {
        Identity::new().above(10.0)
    }

    #[test]
    fn none_until_the_source_first_fires() {
        let mut bars = BarsSince::new(spike());
        assert_eq!(bars.update(1.0), None);
        assert_eq!(bars.update(2.0), None);
        assert_eq!(bars.update(3.0), None);
        assert_eq!(bars.update(42.0), Some(0.0));
    }

    #[test]
    fn counts_bars_and_restarts_on_each_fire() {
        let mut bars = BarsSince::new(spike());
        assert_eq!(bars.update(42.0), Some(0.0));
        assert_eq!(bars.update(1.0), Some(1.0));
        assert_eq!(bars.update(1.0), Some(2.0));
        assert_eq!(bars.update(99.0), Some(0.0));
        assert_eq!(bars.update(1.0), Some(1.0));
    }

    #[test]
    fn a_never_firing_source_never_produces_a_value() {
        let mut bars = BarsSince::new(ValueBool::<Real>::new(false));
        for _ in 0..50 {
            assert_eq!(bars.update(1.0), None);
        }
    }

    #[test]
    fn reset_returns_to_never_fired_not_to_zero() {
        let mut bars = BarsSince::new(spike());
        assert_eq!(bars.update(42.0), Some(0.0));
        bars.reset();
        // `Some(0.0)` here would falsely claim the source fired on this bar.
        assert_eq!(bars.value(), None);
        assert_eq!(bars.update(1.0), None);
    }

    #[test]
    fn threshold_comparisons_read_false_before_the_first_fire() {
        // The safety property the `None` choice buys: a freshness filter can't
        // let a never-fired signal through, and a time-stop can't fire early.
        let mut fresh = BarsSince::new(spike()).below(5.0);
        let mut stale = BarsSince::new(spike()).above(2.0);
        assert_eq!(fresh.update(1.0), None);
        assert_eq!(stale.update(1.0), None);
        assert!(!fresh.is_true());
        assert!(!stale.is_true());
    }

    #[test]
    fn unsettled_source_ticks_do_not_advance_the_counter() {
        // `Log` emits `None` on a non-positive input even after warm-up, so it
        // is the natural stand-in for a mid-stream unsettled source.
        let ln = crate::indicators::Log::new(Identity::new(), std::f64::consts::E);
        let mut bars = BarsSince::new(ln.above(0.0));
        assert_eq!(bars.update(std::f64::consts::E), Some(0.0)); // ln(e) = 1 > 0
        assert_eq!(bars.update(-1.0), Some(0.0)); // no observation, no advance
        assert_eq!(bars.update(1.0), Some(1.0)); // ln(1) = 0, not above 0
    }

    #[test]
    fn high_reports_the_argmax_offset() {
        let mut bars = BarsSinceHigh::new(Identity::new(), 3);
        assert_eq!(bars.update(5.0), None);
        assert_eq!(bars.update(3.0), None);
        assert_eq!(bars.update(1.0), Some(2.0));
        assert_eq!(bars.update(9.0), Some(0.0));
        assert_eq!(bars.update(2.0), Some(1.0));
    }

    #[test]
    fn low_reports_the_argmin_offset() {
        let mut bars = BarsSinceLow::new(Identity::new(), 3);
        assert_eq!(bars.update(1.0), None);
        assert_eq!(bars.update(3.0), None);
        assert_eq!(bars.update(5.0), Some(2.0));
        assert_eq!(bars.update(0.0), Some(0.0));
    }

    #[test]
    fn ties_resolve_to_the_most_recent_occurrence() {
        let mut bars = BarsSinceHigh::new(Identity::new(), 3);
        bars.update(5.0);
        bars.update(5.0);
        assert_eq!(bars.update(5.0), Some(0.0));
    }

    #[test]
    fn extremum_shorthands_stay_within_the_window() {
        let mut bars = BarsSinceHigh::new(Identity::new(), 4);
        // Descending series: the high keeps ageing until it leaves the window,
        // so the reading saturates at `period - 1` rather than growing.
        let out: Vec<_> = (0..10).map(|i| bars.update(100.0 - i as Real)).collect();
        for v in out.iter().skip(3) {
            assert_eq!(*v, Some(3.0));
        }
    }

    #[test]
    fn aroon_up_is_expressible_from_bars_since_high() {
        // The identity from the module docs: Aroon Up = 100·(p − since)/p over
        // a `period + 1` window (fugazi's `Aroon` spans the current bar plus
        // the `period` before it).
        use crate::indicators::{Aroon, Current};
        use crate::types::Candle;

        let closes = [10.0, 12.0, 11.0, 9.0, 14.0, 13.0, 8.0, 7.0];
        let bar = |c: Real| Candle::new(c, c, c, c, 1.0);

        let period = 4;
        let mut aroon = Aroon::new(Current::candle(), period);
        let mut since = BarsSinceHigh::new(Current::close(), period + 1);

        for c in closes {
            let a = aroon.update(bar(c).into());
            let s = since.update(bar(c).into());
            if let (Some(a), Some(s)) = (a, s) {
                let up = 100.0 * (period as Real - s) / period as Real;
                assert!((a.up - up).abs() < 1e-12, "aroon up {} vs {up}", a.up);
            }
        }
    }
}
