//! [`WeightPolicy`]: how a [`Portfolio`](super::Portfolio) decides what
//! fraction of aggregate equity each child should hold, plus the two
//! stateless built-ins ([`Fixed`], [`EqualWeight`]).
//!
//! A weight policy is polled twice per bar by [`Portfolio`]: once as
//! [`observe`](WeightPolicy::observe) — every child's per-bar equity /
//! funds so the policy can accumulate whatever state it needs — and once
//! as [`weights`](WeightPolicy::weights) — the current target weights,
//! read whenever the [`rebalance_on`](super::PortfolioBuilder::rebalance_on)
//! gate fires. Weights are magnitudes (positive), don't have to sum to
//! `1.0` at read time — the portfolio normalizes on use.
//!
//! Every non-trivial policy — inverse-vol, performance-weighted, risk
//! parity — reduces to "keep rolling stats over per-child returns,
//! read a weight out of them on demand", so the trait deliberately keeps
//! the sample struct small: one `equity` reading per child per bar. Add
//! fields (e.g. `positions_value`) only when a shipped policy actually
//! needs one.

use crate::types::Real;

/// The per-child slice a [`WeightPolicy`] sees each bar.
///
/// The two most useful facts a policy could differentiate on: the child's
/// marked-to-market **equity** (funds + positions), and its cash **funds**
/// left to deploy. A performance-based policy reads the equity return
/// bar over bar; a vol-target policy reads its stddev; a risk-parity
/// policy reads its variance. Cash is here for the `CashOnly` rebalance
/// planner (a future addition).
#[derive(Debug, Clone, Copy)]
pub struct ChildSample {
    /// Total equity of the child's sub-wallet — funds plus every position
    /// marked to its last-fed close.
    pub equity: Real,
    /// The cash balance of the child's sub-wallet.
    pub funds: Real,
}

/// A rule for turning per-child equity readings into target weights.
///
/// See the module doc for how a portfolio drives one. Implementors are
/// usually stateful (rolling stats), so [`observe`](Self::observe) takes
/// `&mut self`; [`weights`](Self::weights) is a pure read from that state
/// and takes `&self`. The `n: usize` parameter on `weights` lets stateless
/// policies (e.g. [`EqualWeight`]) return an `n`-vector without carrying
/// the child count in their own state; policies that store an explicit
/// weight vector (e.g. [`Fixed`]) assert the requested `n` matches.
pub trait WeightPolicy: 'static {
    /// Fold this bar's per-child equity readings into whatever rolling
    /// state the policy carries. Called once per bar by the portfolio,
    /// after the wallet is marked to market and before
    /// [`weights`](Self::weights) is queried on a rebalance-fire bar.
    ///
    /// Default: no-op — stateless policies don't need to see samples.
    fn observe(&mut self, samples: &[ChildSample]) {
        let _ = samples;
    }

    /// The current target weights, in child-index order, of length `n`.
    /// Read by the portfolio on rebalance-fire bars. Weights are
    /// magnitudes and needn't sum to `1.0` — the portfolio normalizes on
    /// use. A weight of `0.0` means "flat that child on this rebalance".
    fn weights(&self, n: usize) -> Vec<Real>;

    /// Extra bars the policy needs before its [`weights`](Self::weights)
    /// are meaningful — a rolling-vol policy needs its window filled, a
    /// performance-weighted policy needs enough closed trades. Defaults
    /// to `0` (stateless policy is ready from bar 0).
    ///
    /// The portfolio adds this to its own bars-seen count when computing
    /// [`is_ready`](crate::Strategy::is_ready).
    fn warm_up_period(&self) -> usize {
        0
    }

    /// Clear any rolling state the policy carries. Called from
    /// [`Portfolio::reset`](super::Portfolio). Defaults to no-op.
    fn reset(&mut self) {}
}

/// A weight policy that always returns the same weights, ignoring every
/// child's performance.
///
/// The mechanical default — think of it as "the portfolio held exactly
/// these fractions at inception and never rebalances the split". Panics
/// at [`weights`](WeightPolicy::weights) if the requested `n` doesn't
/// match the length of the stored vector, so a mismatched
/// `Portfolio::builder().add(...).weights(Fixed::new(...))` fails loudly
/// rather than silently mis-scaling.
///
/// ```
/// use fugazi::portfolio::policy::{Fixed, WeightPolicy};
/// let w = Fixed::new(vec![0.6, 0.4]);
/// assert_eq!(w.weights(2), vec![0.6, 0.4]);
/// ```
#[derive(Debug, Clone)]
pub struct Fixed {
    weights: Vec<Real>,
}

impl Fixed {
    /// A fixed-weight policy holding exactly these weights, in child-index
    /// order.
    pub fn new(weights: Vec<Real>) -> Self {
        Self { weights }
    }
}

impl WeightPolicy for Fixed {
    fn weights(&self, n: usize) -> Vec<Real> {
        assert_eq!(
            self.weights.len(),
            n,
            "Fixed::weights: policy holds {} weights but portfolio has {n} children",
            self.weights.len()
        );
        self.weights.clone()
    }
}

/// A weight policy that returns `1/n` for every child — equal split
/// across the universe, regardless of child performance.
///
/// The natural first-cut default when you don't care about relative
/// sizing. Stateless: doesn't need to see samples, doesn't need
/// [`WeightPolicy::reset`](WeightPolicy).
///
/// ```
/// use fugazi::portfolio::policy::{EqualWeight, WeightPolicy};
/// let w = EqualWeight;
/// assert_eq!(w.weights(4), vec![0.25, 0.25, 0.25, 0.25]);
/// ```
#[derive(Debug, Clone, Copy, Default)]
pub struct EqualWeight;

impl WeightPolicy for EqualWeight {
    fn weights(&self, n: usize) -> Vec<Real> {
        if n == 0 {
            return Vec::new();
        }
        let w = 1.0 / (n as Real);
        vec![w; n]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_returns_stored_vector() {
        let p = Fixed::new(vec![0.7, 0.3]);
        assert_eq!(p.weights(2), vec![0.7, 0.3]);
    }

    #[test]
    #[should_panic]
    fn fixed_panics_on_size_mismatch() {
        let p = Fixed::new(vec![0.5, 0.5]);
        let _ = p.weights(3);
    }

    #[test]
    fn equal_weight_returns_uniform() {
        let p = EqualWeight;
        assert_eq!(p.weights(0), Vec::<Real>::new());
        assert_eq!(p.weights(1), vec![1.0]);
        assert_eq!(p.weights(4), vec![0.25; 4]);
    }
}
