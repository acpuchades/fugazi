//! Internal scalar recurrence helpers shared by the smoothing indicators.
//!
//! These operate on a plain `Real` stream (no source, no `Indicator` impl) so
//! that source-wrapping indicators ([`Ema`](super::Ema), [`Rsi`](super::Rsi),
//! [`Macd`](super::Macd), [`Adx`](super::Adx), …) can embed one or more without
//! re-deriving the math.

use crate::types::Real;

/// Residual seed weight below which a recursive smoother is considered settled
/// (0.1%) — the tolerance behind every
/// [`unstable_period`](crate::Indicator::unstable_period) in the crate.
pub(crate) const SETTLE_TOLERANCE: Real = 1e-3;

/// Samples until a geometric decay factor falls below [`SETTLE_TOLERANCE`]:
/// the smallest `k` with `decay^k <= SETTLE_TOLERANCE`.
pub(crate) fn unstable_period(decay: Real) -> usize {
    if decay <= 0.0 {
        0
    } else {
        (SETTLE_TOLERANCE.ln() / decay.ln()).ceil() as usize
    }
}

/// EMA recurrence; seeds on the first sample, then
/// `ema = alpha * x + (1 - alpha) * prev`.
#[derive(Debug, Clone)]
pub(crate) struct EmaState {
    alpha: Real,
    pub value: Option<Real>,
}

impl EmaState {
    pub fn new(period: usize) -> Self {
        assert!(period > 0, "EMA period must be greater than zero");
        Self::with_alpha(2.0 / (period as Real + 1.0))
    }

    pub fn with_alpha(alpha: Real) -> Self {
        assert!(alpha > 0.0 && alpha <= 1.0, "alpha must be in (0, 1]");
        Self { alpha, value: None }
    }

    pub fn update(&mut self, input: Real) -> Option<Real> {
        let next = match self.value {
            Some(prev) => self.alpha * input + (1.0 - self.alpha) * prev,
            None => input,
        };
        self.value = Some(next);
        self.value
    }

    /// Samples after seeding until the seed's weight, `(1 - alpha)^k`, decays
    /// below [`SETTLE_TOLERANCE`] — this state's contribution to an
    /// [`unstable_period`](crate::Indicator::unstable_period).
    pub fn unstable_period(&self) -> usize {
        unstable_period(1.0 - self.alpha)
    }

    pub fn reset(&mut self) {
        self.value = None;
    }
}

/// Wilder smoothing (RMA / SMMA) recurrence; seeds with the mean of the first
/// `period` samples, then `rma = (prev * (period - 1) + x) / period`.
#[derive(Debug, Clone)]
pub(crate) struct WilderState {
    period: usize,
    seen: usize,
    sum: Real,
    pub value: Option<Real>,
}

impl WilderState {
    pub fn new(period: usize) -> Self {
        assert!(period > 0, "period must be greater than zero");
        Self {
            period,
            seen: 0,
            sum: 0.0,
            value: None,
        }
    }

    pub fn period(&self) -> usize {
        self.period
    }

    pub fn update(&mut self, input: Real) -> Option<Real> {
        match self.value {
            Some(prev) => {
                let p = self.period as Real;
                self.value = Some((prev * (p - 1.0) + input) / p);
            }
            None => {
                self.seen += 1;
                self.sum += input;
                if self.seen == self.period {
                    self.value = Some(self.sum / self.period as Real);
                }
            }
        }
        self.value
    }

    /// Samples after seeding until the seed's weight, `((period - 1)/period)^k`,
    /// decays below [`SETTLE_TOLERANCE`] — this state's contribution to an
    /// [`unstable_period`](crate::Indicator::unstable_period).
    pub fn unstable_period(&self) -> usize {
        unstable_period((self.period as Real - 1.0) / self.period as Real)
    }

    pub fn reset(&mut self) {
        self.seen = 0;
        self.sum = 0.0;
        self.value = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settle_counts_decay_steps() {
        // decay 0.5: 0.5^10 ≈ 9.8e-4 <= 1e-3 < 0.5^9.
        assert_eq!(unstable_period(0.5), 10);
        // alpha = 1 (no memory): settled immediately.
        assert_eq!(unstable_period(0.0), 0);
    }

    #[test]
    fn ema_and_wilder_settle_match_their_decay() {
        // EMA period 3: alpha = 0.5, so decay 0.5 -> 10 steps.
        assert_eq!(EmaState::new(3).unstable_period(), 10);
        // Degenerate alpha = 1 keeps no seed at all.
        assert_eq!(EmaState::with_alpha(1.0).unstable_period(), 0);
        // Wilder period 1 averages nothing (alpha = 1).
        assert_eq!(WilderState::new(1).unstable_period(), 0);
        // Wilder period 14: (13/14)^k <= 1e-3 at k = 94.
        assert_eq!(WilderState::new(14).unstable_period(), 94);
    }
}
