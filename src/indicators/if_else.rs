//! [`IfElse`]: a three-source ternary — pick one of two `Real` sources per
//! bar based on a `bool` source's reading.
//!
//! The idiomatic branching primitive. Ships the "ADX-gated momentum score"
//! shape without needing a bespoke source: `adx.gt(25.0).if_else(roc,
//! Value::new(0.0))` reads exactly like the English sentence.

use crate::indicator::Indicator;
use crate::types::Real;

/// A three-source ternary. Reads `condition` each bar and returns:
///
/// * `Some(if_true.value())` when the condition is `Some(true)` — which may
///   itself be `None` if that branch is still warming;
/// * `Some(if_false.value())` on `Some(false)`, likewise;
/// * `None` when the condition is `None` — unsettled control propagates
///   the same way it does elsewhere in the crate.
///
/// All three sources are advanced every bar — never short-circuited on the
/// non-selected branch. A branch that doesn't fire this bar keeps its own
/// warm-up progressing, so a future flip to it reads its properly-settled
/// value rather than a fresh unwarm one. This matches [`Combine`]'s
/// convention for the same reason.
///
/// # Warm-up
///
/// [`warm_up_period`](Indicator::warm_up_period) is the max of all three
/// sources' warm-ups (the *conservative worst case* — safe regardless of
/// which branch a condition happens to select). [`stable_period`] is
/// likewise the max of the three, so a downstream consumer waiting for
/// full readiness (e.g. a strategy's `is_ready()`) waits long enough for
/// any branch to have settled.
///
/// **The actual first `Some` can arrive earlier than `warm_up_period()`**:
/// when the condition is settled and the *selected* branch is settled
/// (both cases even if the *unselected* branch isn't). The ternary
/// deliberately doesn't gate on a `bars_seen` counter — output is
/// available as soon as the currently-active path can produce it.
/// `warm_up_period()` is therefore an *upper bound* on the first-`Some`
/// sample, not the exact position, so this indicator is
/// intentionally excluded from the `tests/warm_up.rs` exact-warm-up
/// battery (which asserts "first `Some` at exactly sample N").
///
/// [`stable_period`]: Indicator::stable_period
///
/// # Example
///
/// ```
/// use fugazi::prelude::*;
/// use fugazi::indicators::{IfElse, Value};
///
/// // Constant-true condition: the true branch always wins.
/// let cond = Value::<Real>::new(1.0).above(0.5);
/// let mut ind = IfElse::new(cond, Value::<Real>::new(42.0), Value::<Real>::new(-1.0));
/// assert_eq!(ind.update(0.0), Some(42.0));
/// ```
///
/// [`Combine`]: crate::indicators::Combine
#[derive(Debug, Clone)]
pub struct IfElse<Cond, T, F> {
    condition: Cond,
    if_true: T,
    if_false: F,
    /// Latest selected value. `None` while the condition is `None`, or
    /// while the currently-selected branch is `None`.
    pub value: Option<Real>,
}

impl<Cond, T, F> IfElse<Cond, T, F> {
    /// Build a ternary that reads `condition` each bar and returns
    /// `if_true`'s or `if_false`'s value.
    pub fn new(condition: Cond, if_true: T, if_false: F) -> Self {
        Self {
            condition,
            if_true,
            if_false,
            value: None,
        }
    }
}

impl<I, Cond, T, F> Indicator for IfElse<Cond, T, F>
where
    I: Clone,
    Cond: Indicator<Input = I, Output = bool>,
    T: Indicator<Input = I, Output = Real>,
    F: Indicator<Input = I, Output = Real>,
{
    type Input = I;
    type Output = Real;

    fn update(&mut self, input: I) -> Option<Real> {
        // Advance all three unconditionally so a branch that doesn't fire
        // this bar keeps its warm-up progressing.
        let cond = self.condition.update(input.clone());
        let true_v = self.if_true.update(input.clone());
        let false_v = self.if_false.update(input);
        // Natural semantics: `None` on `None` cond, otherwise the selected
        // branch's reading (which may itself still be `None` while warming).
        self.value = match cond {
            Some(true) => true_v,
            Some(false) => false_v,
            None => None,
        };
        self.value
    }

    fn value(&self) -> Option<Real> {
        self.value
    }

    fn warm_up_period(&self) -> usize {
        self.condition
            .warm_up_period()
            .max(self.if_true.warm_up_period())
            .max(self.if_false.warm_up_period())
    }

    fn unstable_period(&self) -> usize {
        // Same shape as `Combine`: total stable = max of the three sources'
        // stable periods; the unstable-period we return is the excess above
        // our own warm-up.
        let stable = self
            .condition
            .stable_period()
            .max(self.if_true.stable_period())
            .max(self.if_false.stable_period());
        stable - self.warm_up_period()
    }

    fn reset(&mut self) {
        self.condition.reset();
        self.if_true.reset();
        self.if_false.reset();
        self.value = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indicators::{IndicatorExt, Sma, Value};

    /// A single-source `Indicator` that emits its input on the Nth call and
    /// `None` before/after — used to script a bool condition that flips on
    /// a chosen sample.
    struct AtBar {
        seen: usize,
        fires_on: usize,
    }
    impl Indicator for AtBar {
        type Input = Real;
        type Output = bool;
        fn update(&mut self, _input: Real) -> Option<bool> {
            self.seen += 1;
            Some(self.seen >= self.fires_on)
        }
        fn value(&self) -> Option<bool> {
            Some(self.seen >= self.fires_on)
        }
        fn warm_up_period(&self) -> usize {
            1
        }
        fn reset(&mut self) {
            self.seen = 0;
        }
    }

    #[test]
    fn true_branch_selected_on_true_condition() {
        let mut ind = IfElse::new(
            Value::<Real>::new(1.0).above(0.5),
            Value::<Real>::new(42.0),
            Value::<Real>::new(-1.0),
        );
        assert_eq!(ind.update(0.0), Some(42.0));
        assert_eq!(ind.update(100.0), Some(42.0));
    }

    #[test]
    fn false_branch_selected_on_false_condition() {
        let mut ind = IfElse::new(
            Value::<Real>::new(1.0).below(0.5),
            Value::<Real>::new(42.0),
            Value::<Real>::new(-1.0),
        );
        assert_eq!(ind.update(0.0), Some(-1.0));
    }

    #[test]
    fn cond_none_propagates_none() {
        // A SMA-3 over the condition input — for the first two calls it's
        // still warming and reads None; the ternary follows and emits None,
        // ignoring both branches which have settled Value inputs.
        struct WarmingBool {
            n: usize,
        }
        impl Indicator for WarmingBool {
            type Input = Real;
            type Output = bool;
            fn update(&mut self, _input: Real) -> Option<bool> {
                self.n += 1;
                if self.n >= 3 { Some(true) } else { None }
            }
            fn value(&self) -> Option<bool> {
                if self.n >= 3 { Some(true) } else { None }
            }
            fn warm_up_period(&self) -> usize {
                3
            }
            fn reset(&mut self) {
                self.n = 0;
            }
        }
        let mut ind = IfElse::new(
            WarmingBool { n: 0 },
            Value::<Real>::new(1.0),
            Value::<Real>::new(2.0),
        );
        assert_eq!(ind.update(0.0), None);
        assert_eq!(ind.update(0.0), None);
        assert_eq!(ind.update(0.0), Some(1.0));
    }

    #[test]
    fn selected_branch_none_propagates() {
        // Condition true → we pick if_true; if_true is an SMA-5, still warming,
        // so the ternary reads None even though if_false would have been Some.
        let mut ind = IfElse::new(
            Value::<Real>::new(1.0).above(0.5),
            Sma::new(crate::indicators::Identity::<Real>::new(), 5),
            Value::<Real>::new(99.0),
        );
        for _ in 0..4 {
            assert_eq!(ind.update(1.0), None);
        }
        // Fifth sample warms the SMA-5.
        assert!(ind.update(1.0).is_some());
    }

    #[test]
    fn non_selected_branch_still_advances() {
        // Both branches are warming SMA-3s. Toggle the condition each bar via
        // AtBar (Some(false) until bar 4, then Some(true)). The if_true branch
        // isn't consulted until bar 4, but by that bar it should read Some —
        // its warm-up has advanced silently.
        let mut ind = IfElse::new(
            AtBar { seen: 0, fires_on: 4 },
            Sma::new(crate::indicators::Identity::<Real>::new(), 3),
            Sma::new(crate::indicators::Identity::<Real>::new(), 3),
        );
        // Bars 1..=3: cond is false, so we read if_false (SMA-3) — warming.
        assert_eq!(ind.update(10.0), None);
        assert_eq!(ind.update(20.0), None);
        assert_eq!(ind.update(30.0), Some(20.0)); // if_false's SMA-3 = (10+20+30)/3
        // Bar 4: cond flips to true; if_true has also seen bars 1..=3, so its
        // SMA-3 is also settled.
        assert_eq!(ind.update(40.0), Some((20.0 + 30.0 + 40.0) / 3.0));
    }

    #[test]
    fn publishes_early_when_selected_branch_warms_before_the_max() {
        // Cond is fast (Const true), if_true is slow (SMA-5), if_false is
        // fast (Value). `warm_up_period()` is 5, but the ternary picks
        // `if_false` and can publish on bar 1 — the "actual first Some
        // can arrive earlier than warm_up_period" contract.
        let mut ind = IfElse::new(
            Value::<Real>::new(1.0).below(0.5), // Const false
            Sma::new(crate::indicators::Identity::<Real>::new(), 5),
            Value::<Real>::new(-1.0),
        );
        assert_eq!(ind.warm_up_period(), 5);
        assert_eq!(ind.update(1.0), Some(-1.0));
    }

    #[test]
    fn warm_up_is_max_of_three_sources() {
        // Cond warms in 1, if_true in 5, if_false in 2 → overall = 5.
        let ind = IfElse::new(
            Value::<Real>::new(1.0).above(0.5), // warm-up 0
            Sma::new(crate::indicators::Identity::<Real>::new(), 5), // 5
            Sma::new(crate::indicators::Identity::<Real>::new(), 2), // 2
        );
        assert_eq!(ind.warm_up_period(), 5);
    }

    #[test]
    fn reset_clears_the_ternary_and_its_sources() {
        let mut ind = IfElse::new(
            AtBar { seen: 0, fires_on: 1 },
            Value::<Real>::new(1.0),
            Value::<Real>::new(2.0),
        );
        ind.update(0.0);
        ind.update(0.0);
        assert!(ind.value.is_some());
        ind.reset();
        assert!(ind.value.is_none());
        // Condition source is reset too; AtBar starts firing again on bar 1.
        assert_eq!(ind.update(0.0), Some(1.0));
    }
}
