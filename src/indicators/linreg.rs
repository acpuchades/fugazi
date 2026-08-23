//! Rolling least-squares fit of a source against time.

use fugazi_derive::SaveState;

use crate::indicator::Indicator;
use crate::indicators::stats::WindowCovariance;
use crate::types::Real;

/// The four readings of a [`LinReg`] fit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LinRegValue {
    /// Slope of the fitted line, in source units **per bar**.
    pub slope: Real,
    /// The fitted line evaluated at the oldest bar in the window.
    pub intercept: Real,
    /// The fitted line evaluated at the newest bar in the window.
    pub value: Real,
    /// Coefficient of determination, in `[0, 1]`: how much of the source's
    /// variation the straight line accounts for.
    pub r2: Real,
}

/// Rolling least-squares fit of a source against the bar index.
///
/// The trend primitive an expression grammar otherwise cannot spell: a slope
/// needs a regression against time, and there is no composition of lagged
/// differences that produces one. Built on the shared `WindowCovariance` core
/// with the bar index as the `x` leg — O(1) to update, one centred O(period)
/// pass to read all four outputs together.
///
/// Classic use is the pair: `slope · r2`, a momentum score that discounts a
/// steep fit nothing actually follows. `slope / value` annualises to a
/// scale-free trend rate, which is what makes it comparable across instruments.
///
/// **`x` is the bar index, not the sample ordinal.** A bar the source declines
/// pushes nothing, so a gap widens the spacing between retained points rather
/// than closing it — the slope stays *per bar*, and a source that quotes half
/// the time reads half the slope rather than a silently rescaled one. That is
/// why [`intercept`](LinRegValue::intercept) is the fit at the window's oldest
/// *retained* bar and cannot be derived from the period alone.
///
/// Both ends of the line are reported because they answer different questions:
/// [`value`](LinRegValue::value) is the de-noised level *now* (TA-Lib's
/// `LINEARREG`), [`intercept`](LinRegValue::intercept) the level the fit
/// started from (`LINEARREG_INTERCEPT`).
#[derive(Debug, Clone, SaveState)]
pub struct LinReg<S> {
    #[state(source)]
    source: S,
    #[state(core)]
    cov: WindowCovariance,
    /// The bar index of the *next* sample — advanced on every update, whether
    /// or not the source produced a value.
    bar: Real,
    /// Latest slope.
    pub slope: Option<Real>,
    /// Latest intercept (the fit at the oldest retained bar).
    pub intercept: Option<Real>,
    /// Latest fitted value (the fit at the newest retained bar).
    pub value: Option<Real>,
    /// Latest coefficient of determination.
    pub r2: Option<Real>,
}

impl<S> LinReg<S> {
    /// Fit over a `period`-bar window.
    ///
    /// # Panics
    /// Panics if `period` is less than 2 — one point has no slope, and the
    /// degenerate fit would report `0.0` rather than saying so.
    pub fn new(source: S, period: usize) -> Self {
        assert!(
            period > 1,
            "linear regression period must be at least 2, got {period}",
        );
        Self {
            source,
            cov: WindowCovariance::new(period),
            bar: 0.0,
            slope: None,
            intercept: None,
            value: None,
            r2: None,
        }
    }

    pub fn period(&self) -> usize {
        self.cov.period()
    }
}

// Component accessors: each reading as a standalone `Indicator<Output = Real>`.
crate::indicators::component::component_accessors!(
    LinReg<S>, LinRegValue;
    /// The fitted slope, in source units per bar, as a standalone source.
    slope => slope,
    /// The fit at the window's oldest bar, as a standalone source.
    intercept => intercept,
    /// The fit at the window's newest bar, as a standalone source.
    value => value,
    /// The fit's coefficient of determination, as a standalone source.
    r2 => r2,
);

impl<S: Indicator<Output = Real>> Indicator for LinReg<S> {
    type Input = S::Input;
    type Output = LinRegValue;

    fn update(&mut self, input: Self::Input) -> Option<LinRegValue> {
        let x = self.bar;
        self.bar += 1.0;
        let ready = match self.source.update(input) {
            Some(y) => self.cov.update(x, y),
            None => false,
        };

        if ready {
            // One pass for all four readings — asking the core twice would scan
            // the window twice for the same numbers.
            let m = self.cov.moments();
            let slope = m.slope_y_on_x();
            // `ŷ(t) = ȳ + slope·(t − x̄)`, evaluated at each end of the window.
            // Anchoring on the means rather than on a stored intercept keeps
            // both ends exact when the retained bars are unevenly spaced.
            let oldest = self.cov.oldest_x().unwrap_or(x);
            let intercept = m.mean_y + slope * (oldest - m.mean_x);
            let value = m.mean_y + slope * (x - m.mean_x);
            let out = LinRegValue {
                slope,
                intercept,
                value,
                r2: m.r_squared(),
            };
            self.slope = Some(out.slope);
            self.intercept = Some(out.intercept);
            self.value = Some(out.value);
            self.r2 = Some(out.r2);
            Some(out)
        } else {
            self.slope = None;
            self.intercept = None;
            self.value = None;
            self.r2 = None;
            None
        }
    }

    fn value(&self) -> Option<LinRegValue> {
        match (self.slope, self.intercept, self.value, self.r2) {
            (Some(slope), Some(intercept), Some(value), Some(r2)) => Some(LinRegValue {
                slope,
                intercept,
                value,
                r2,
            }),
            _ => None,
        }
    }

    fn warm_up_bars(&self) -> usize {
        self.source.warm_up_bars().max(1) + self.cov.period() - 1
    }

    fn unstable_bars(&self) -> usize {
        self.source.unstable_bars()
    }

    fn reset(&mut self) {
        self.source.reset();
        self.cov.reset();
        self.bar = 0.0;
        self.slope = None;
        self.intercept = None;
        self.value = None;
        self.r2 = None;
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
    use crate::indicators::{Identity, Sma};

    /// A perfect ramp `y = 2x + 1` over bars 0,1,2: slope 2, the fit at the
    /// oldest bar (0) is 1, at the newest (2) is 5, and a straight line
    /// explains all of it (`r² = 1`).
    #[test]
    fn a_perfect_ramp_is_recovered_exactly() {
        let mut lr = LinReg::new(Identity::new(), 3);
        assert_eq!(lr.update(1.0), None);
        assert_eq!(lr.update(3.0), None);
        let out = lr.update(5.0).unwrap();
        assert!((out.slope - 2.0).abs() < 1e-12, "slope {}", out.slope);
        assert!(
            (out.intercept - 1.0).abs() < 1e-12,
            "intercept {}",
            out.intercept
        );
        assert!((out.value - 5.0).abs() < 1e-12, "value {}", out.value);
        assert!((out.r2 - 1.0).abs() < 1e-12, "r2 {}", out.r2);
    }

    /// A falling line reads a negative slope, and `r²` — a square — stays 1.
    #[test]
    fn a_falling_line_reads_a_negative_slope() {
        let mut lr = LinReg::new(Identity::new(), 3);
        lr.update(10.0);
        lr.update(8.0);
        let out = lr.update(6.0).unwrap();
        assert!((out.slope + 2.0).abs() < 1e-12, "slope {}", out.slope);
        assert!((out.r2 - 1.0).abs() < 1e-12, "r2 {}", out.r2);
    }

    /// A flat source has no trend: slope 0, and both ends of the fit sit on the
    /// level itself. `r²` is 0 — the line explains none of a variation that
    /// isn't there.
    #[test]
    fn a_flat_source_has_no_slope() {
        let mut lr = LinReg::new(Identity::new(), 3);
        lr.update(4.0);
        lr.update(4.0);
        let out = lr.update(4.0).unwrap();
        assert_eq!(out.slope, 0.0);
        assert_eq!(out.r2, 0.0);
        assert!((out.value - 4.0).abs() < 1e-12, "value {}", out.value);
    }

    /// `y = 1,3,5,4` fitted over the last three points (3,5,4 at bars 1,2,3):
    /// x̄ = 2, ȳ = 4, cov = ((−1)(−1) + 0·1 + 1·0)/3 = 1/3, varₓ = 2/3, so the
    /// slope is 0.5. The fit at bar 3 is 4 + 0.5·1 = 4.5, at bar 1 it's 3.5.
    #[test]
    fn a_noisy_window_fits_by_least_squares() {
        let mut lr = LinReg::new(Identity::new(), 3);
        lr.update(1.0);
        lr.update(3.0);
        lr.update(5.0);
        let out = lr.update(4.0).unwrap();
        assert!((out.slope - 0.5).abs() < 1e-12, "slope {}", out.slope);
        assert!((out.value - 4.5).abs() < 1e-12, "value {}", out.value);
        assert!(
            (out.intercept - 3.5).abs() < 1e-12,
            "intercept {}",
            out.intercept
        );
    }

    #[test]
    fn warm_up_accounts_for_the_source_and_the_window() {
        // SMA(2) warms at 2, window 3 → 2 + 3 − 1 = 4.
        let lr = LinReg::new(Sma::new(Identity::new(), 2), 3);
        assert_eq!(lr.warm_up_bars(), 4);
    }

    #[test]
    #[should_panic(expected = "at least 2")]
    fn a_single_point_has_no_slope() {
        let _ = LinReg::new(Identity::<Real>::new(), 1);
    }
}
