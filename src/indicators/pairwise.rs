//! Rolling two-source statistics: correlation, covariance, beta.
//!
//! One carrier ([`PairStat`]) over the shared `WindowCovariance` core, driven
//! by an operator that picks a reading out of the window's [`Moments`]. The
//! three differ in that one line and nothing else — same window, same warm-up,
//! same O(1) update and single centred O(period) read.

use std::marker::PhantomData;

use fugazi_derive::SaveState;

use crate::indicator::Indicator;
use crate::indicators::stats::{Moments, WindowCovariance};
use crate::types::Real;

/// Which reading of a paired window a [`PairStat`] reports.
///
/// A zero-sized marker, so the carrier holds it as a `PhantomData` and the
/// method is an associated function. `moments` has already made the one centred
/// pass; an implementor divides, it does not re-scan.
pub trait PairStatOp {
    /// The statistic, read out of the window's moments.
    fn read(moments: &Moments) -> Real;
}

/// Rolling Pearson correlation of the two legs, in `[-1, 1]`.
#[derive(Debug, Clone, Copy)]
pub struct CorrelationOp;
impl PairStatOp for CorrelationOp {
    fn read(moments: &Moments) -> Real {
        moments.correlation()
    }
}

/// Rolling population covariance of the two legs.
///
/// The unnormalised sibling of correlation: it keeps the units (a covariance of
/// two price series is in price², of two return series in return²) and so it
/// carries magnitude information correlation throws away. Divide by the product
/// of the standard deviations to recover the correlation.
#[derive(Debug, Clone, Copy)]
pub struct CovarianceOp;
impl PairStatOp for CovarianceOp {
    fn read(moments: &Moments) -> Real {
        moments.cov
    }
}

/// Rolling beta of the **left** leg against the **right**: `cov / var_rhs`.
///
/// The slope of the least-squares line explaining `lhs` by `rhs`, so the
/// argument order is "asset first, benchmark second" — `Beta::new(asset,
/// benchmark, 60)`. Feed returns, not prices, unless you specifically want the
/// price-level relationship: beta is conventionally a return-space quantity,
/// and this primitive takes whatever it is handed rather than differencing
/// behind your back.
///
/// `0.0` when the benchmark leg is dispersion-free over the window — a flat
/// benchmark measures no sensitivity, and the alternative is an infinity in a
/// position size.
#[derive(Debug, Clone, Copy)]
pub struct BetaOp;
impl PairStatOp for BetaOp {
    fn read(moments: &Moments) -> Real {
        moments.slope_x_on_y()
    }
}

/// A rolling statistic over two Real sources, parameterised by which reading it
/// reports.
///
/// Use the aliases ([`Correlation`], [`Covariance`], [`Beta`]). Feeds the same
/// input to both sources each step (hence `Input: Clone`) and pairs their
/// outputs over the last `period` samples via the shared `WindowCovariance`
/// core: O(1) to update, one centred O(period) pass to read (see that core's
/// docs for why centred). Produces `None` until both sources are warm *and* the
/// window is full.
///
/// A bar where either leg reads `None` pushes nothing — the window pairs
/// observations, not bar slots, so a listing gap on one leg delays the reading
/// rather than corrupting it.
///
/// One shape, several regime features:
/// - **Cross-asset correlation** — `Correlation::new(Close::of(pick_a),
///   Close::of(pick_b), 30)`: is everything trading as one risk-on/risk-off
///   blob or dispersed.
/// - **Autocorrelation** — `Correlation::new(x.clone(), x.lag(n), period)`:
///   lag-`n` serial correlation, a trending-vs-mean-reverting signal.
/// - **Rolling hedge ratio** — `Beta::new(leg_a_returns, leg_b_returns, 60)`:
///   how many units of the benchmark one unit of the asset moves like.
#[derive(Debug, Clone, SaveState)]
pub struct PairStat<L, R, Op> {
    #[state(source)]
    lhs: L,
    #[state(source)]
    rhs: R,
    #[state(core)]
    cov: WindowCovariance,
    /// Latest reading; `None` until ready.
    pub value: Option<Real>,
    #[state(skip)]
    _op: PhantomData<fn() -> Op>,
}

impl<L, R, Op> PairStat<L, R, Op> {
    /// # Panics
    /// Panics if `period` is zero.
    pub fn new(lhs: L, rhs: R, period: usize) -> Self {
        Self {
            lhs,
            rhs,
            cov: WindowCovariance::new(period),
            value: None,
            _op: PhantomData,
        }
    }

    pub fn period(&self) -> usize {
        self.cov.period()
    }
}

impl<L, R, Op> Indicator for PairStat<L, R, Op>
where
    L: Indicator<Output = Real>,
    R: Indicator<Input = L::Input, Output = Real>,
    L::Input: Clone,
    Op: PairStatOp,
{
    type Input = L::Input;
    type Output = Real;

    fn update(&mut self, input: Self::Input) -> Option<Real> {
        let x = self.lhs.update(input.clone());
        let y = self.rhs.update(input);
        self.value = match (x, y) {
            (Some(x), Some(y)) if self.cov.update(x, y) => Some(Op::read(&self.cov.moments())),
            _ => None,
        };
        self.value
    }

    fn value(&self) -> Option<Real> {
        self.value
    }

    fn warm_up_bars(&self) -> usize {
        // Both legs must be warm before the covariance window starts filling, so
        // the join point is the later of the two warm-ups; the window then needs
        // `period` more samples.
        self.lhs.warm_up_bars().max(self.rhs.warm_up_bars()).max(1) + self.cov.period() - 1
    }

    fn unstable_bars(&self) -> usize {
        // The excess above *this* indicator's own warm-up, the shape `Combine`
        // and `Keltner` use — not `max(unstable)`, which is a different
        // subtraction and over-reports whenever the two legs have different
        // warm-ups. The `+ period - 1` appears on both sides and cancels; it is
        // spelled out so the parallel with `warm_up_bars` is visible.
        let settles =
            self.lhs.stable_bars().max(self.rhs.stable_bars()).max(1) + self.cov.period() - 1;
        settles - self.warm_up_bars()
    }

    fn reset(&mut self) {
        self.lhs.reset();
        self.rhs.reset();
        self.cov.reset();
        self.value = None;
    }

    fn save_state(&self) -> serde_json::Value {
        self.save_state_fields()
    }

    fn load_state(&mut self, state: &serde_json::Value) -> Result<(), String> {
        self.load_state_fields(state)
    }
}

/// Rolling Pearson correlation between two Real sources over a fixed window.
///
/// Reads in `[-1, 1]` once ready, with a dispersion-free leg (either source
/// constant over the window) reading `0.0` — correlation is undefined there.
pub type Correlation<L, R> = PairStat<L, R, CorrelationOp>;
/// Rolling population covariance between two Real sources over a fixed window.
pub type Covariance<L, R> = PairStat<L, R, CovarianceOp>;
/// Rolling beta of the left source against the right over a fixed window.
pub type Beta<L, R> = PairStat<L, R, BetaOp>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indicators::{Identity, IndicatorExt, Sma, Value};

    #[test]
    fn perfectly_correlated_lines_read_one() {
        // y = x fed to both legs: correlation is exactly 1.
        let mut c = Correlation::new(Identity::new(), Identity::new(), 3);
        assert_eq!(c.update(1.0), None);
        assert_eq!(c.update(2.0), None);
        let out = c.update(3.0).unwrap();
        assert!((out - 1.0).abs() < 1e-12, "got {out}");
    }

    #[test]
    fn anti_correlated_lines_read_minus_one() {
        // rhs = -lhs (via `x * -1`), a perfectly negative relationship.
        let mut c = Correlation::new(Identity::new(), Identity::new().mul(Value::new(-1.0)), 3);
        c.update(1.0);
        c.update(2.0);
        let out = c.update(3.0).unwrap();
        assert!((out + 1.0).abs() < 1e-12, "got {out}");
    }

    #[test]
    fn constant_leg_reads_zero() {
        // rhs constant → its window variance is 0 → correlation undefined → 0.
        let mut c = Correlation::new(Identity::new(), Value::new(7.0), 3);
        c.update(1.0);
        c.update(2.0);
        assert_eq!(c.update(3.0), Some(0.0));
    }

    #[test]
    fn warm_up_accounts_for_both_legs_and_window() {
        // lhs SMA(2) warms at 2, window 3 → 2 + 3 − 1 = 4.
        let c = Correlation::new(Sma::new(Identity::new(), 2), Identity::new(), 3);
        assert_eq!(c.warm_up_bars(), 4);
    }

    /// `x = 1,2,3` against itself: mean 2, population variance
    /// `((-1)² + 0 + 1²)/3 = 2/3`, and the covariance of a series with itself
    /// *is* its variance.
    #[test]
    fn covariance_of_a_series_with_itself_is_its_variance() {
        let mut c = Covariance::new(Identity::new(), Identity::new(), 3);
        c.update(1.0);
        c.update(2.0);
        let out = c.update(3.0).unwrap();
        assert!((out - 2.0 / 3.0).abs() < 1e-12, "got {out}");
    }

    /// `lhs = 2·rhs` exactly, so the least-squares slope of lhs on rhs is 2 —
    /// `cov/var_rhs = (2·var_rhs)/var_rhs`.
    #[test]
    fn beta_recovers_a_known_slope() {
        let mut b = Beta::new(Identity::new().mul(Value::new(2.0)), Identity::new(), 3);
        b.update(1.0);
        b.update(2.0);
        let out = b.update(3.0).unwrap();
        assert!((out - 2.0).abs() < 1e-12, "got {out}");
    }

    /// Beta is *not* symmetric in its arguments: `lhs = 2·rhs` gives 2 one way
    /// and 0.5 the other. The guard against silently swapping the operands.
    #[test]
    fn beta_is_directional() {
        let mut b = Beta::new(Identity::new(), Identity::new().mul(Value::new(2.0)), 3);
        b.update(1.0);
        b.update(2.0);
        let out = b.update(3.0).unwrap();
        assert!((out - 0.5).abs() < 1e-12, "got {out}");
    }

    /// A flat benchmark leg measures no sensitivity — `0.0`, not an infinity.
    #[test]
    fn beta_against_a_flat_benchmark_reads_zero() {
        let mut b = Beta::new(Identity::new(), Value::new(7.0), 3);
        b.update(1.0);
        b.update(2.0);
        assert_eq!(b.update(3.0), Some(0.0));
    }

    /// `unstable_bars` is the excess above *this* indicator's warm-up, not the
    /// larger of the legs' own instabilities — two different subtractions that
    /// coincide only when the legs warm up together. A recursive short leg
    /// beside a long windowed one is fully settled long before the covariance
    /// window has filled, and the max-of-unstable form claimed otherwise.
    #[test]
    fn unstable_bars_is_measured_from_this_indicators_own_warm_up() {
        use crate::indicators::{Ema, Sma};

        let long_fir = Sma::new(Identity::<Real>::new(), 60);
        let short_iir = Ema::new(Identity::<Real>::new(), 5);
        let settles = long_fir.stable_bars().max(short_iir.stable_bars());
        // Precondition: the short recursive leg is what carries the instability,
        // and it has already settled by the time the long leg is merely warm.
        assert!(short_iir.unstable_bars() > 0);
        assert!(settles == long_fir.stable_bars());

        let c: Correlation<Sma<Identity<Real>>, Ema<Identity<Real>>> =
            PairStat::new(long_fir, short_iir, 10);
        assert_eq!(c.warm_up_bars(), 60 + 10 - 1);
        assert_eq!(c.stable_bars(), settles + 10 - 1);
        assert_eq!(c.unstable_bars(), c.stable_bars() - c.warm_up_bars());

        // Legs that warm up together are the case the two forms agree on, and
        // must stay agreed.
        let a = Ema::new(Identity::<Real>::new(), 7);
        let b = Ema::new(Identity::<Real>::new(), 7);
        let un = a.unstable_bars();
        let c: Correlation<Ema<Identity<Real>>, Ema<Identity<Real>>> = PairStat::new(a, b, 4);
        assert_eq!(c.unstable_bars(), un);
    }
}
