//! [`Match`]: value-equality dispatch — the Python-`match`-style N-way
//! branch. Reads `on` once per bar and picks the first case whose pattern
//! equals `on`'s reading; falls through to `default` when no case matches.
//!
//! The runtime twin of a nested [`IfElse`](crate::indicators::IfElse)
//! chain — same semantics, but `on` is evaluated once (not once per case),
//! and every branch's warm-up progresses in parallel regardless of which
//! one fires this bar.

use crate::indicator::Indicator;
use crate::types::Real;

/// N-way dispatch by value equality — the runtime primitive behind YAML's
/// `!match` tag. Given a source `on` producing values of type `K` (a
/// scalar, typically `Real` or `Arc<str>`), a list of `(pattern, branch)`
/// cases, and a `default` branch, [`Match`] emits the *first* case whose
/// pattern equals `on`'s current reading, and falls through to `default`
/// otherwise.
///
/// Every branch (cases + default) is advanced every bar — never
/// short-circuited on the non-selected ones — so a branch that doesn't
/// fire this bar keeps its own warm-up progressing. `on` is advanced
/// once. Matches [`IfElse`](crate::indicators::IfElse)'s convention.
///
/// # Warm-up
///
/// [`warm_up_period`](Indicator::warm_up_period) is the max across `on`,
/// every case's branch, and `default` — the conservative worst case,
/// safe regardless of which branch a given `on` reading happens to
/// select. [`stable_period`](Indicator::stable_period) is likewise the
/// max, so a downstream consumer waiting for full readiness waits long
/// enough for every branch to have settled.
///
/// # Example
///
/// ```
/// use fugazi::prelude::*;
/// use fugazi::indicators::{Match, Value, Identity};
///
/// // Dispatch on the input value itself (Identity): 1 → 10, 2 → 20, else 0.
/// let mut ind = Match::new(
///     Identity::<Real>::new(),
///     vec![
///         (1.0, Value::<Real>::new(10.0)),
///         (2.0, Value::<Real>::new(20.0)),
///     ],
///     Value::<Real>::new(0.0),
/// );
/// assert_eq!(ind.update(1.0), Some(10.0));
/// assert_eq!(ind.update(2.0), Some(20.0));
/// assert_eq!(ind.update(3.0), Some(0.0));
/// ```
#[derive(Debug, Clone)]
pub struct Match<S, T, K> {
    on: S,
    cases: Vec<(K, T)>,
    default: T,
    value: Option<Real>,
}

impl<S, T, K> Match<S, T, K> {
    /// Build a match dispatcher. `cases` are checked in order — the first
    /// pattern equal to `on`'s reading fires; ties resolve to the earliest
    /// case, later duplicates are dead code.
    pub fn new(on: S, cases: Vec<(K, T)>, default: T) -> Self {
        Self {
            on,
            cases,
            default,
            value: None,
        }
    }
}

impl<I, S, T, K> Indicator for Match<S, T, K>
where
    I: Clone,
    S: Indicator<Input = I, Output = K>,
    T: Indicator<Input = I, Output = Real>,
    K: PartialEq,
{
    type Input = I;
    type Output = Real;

    fn update(&mut self, input: I) -> Option<Real> {
        // Advance `on` once.
        let on_val = self.on.update(input.clone());
        // Advance every branch unconditionally so the non-selected ones
        // keep warming up. Collect their readings by index so we can pick
        // one after the matching case is identified.
        let branch_vals: Vec<Option<Real>> = self
            .cases
            .iter_mut()
            .map(|(_, br)| br.update(input.clone()))
            .collect();
        let default_val = self.default.update(input);

        self.value = match on_val {
            // `None` on `on` propagates — consistent with `IfElse`'s
            // treatment of an unsettled condition.
            None => None,
            Some(v) => {
                let mut hit = None;
                for (i, (k, _)) in self.cases.iter().enumerate() {
                    if k == &v {
                        hit = Some(branch_vals[i]);
                        break;
                    }
                }
                hit.unwrap_or(default_val)
            }
        };
        self.value
    }

    fn value(&self) -> Option<Real> {
        self.value
    }

    fn warm_up_period(&self) -> usize {
        let mut w = self
            .on
            .warm_up_period()
            .max(self.default.warm_up_period());
        for (_, br) in &self.cases {
            w = w.max(br.warm_up_period());
        }
        w
    }

    fn unstable_period(&self) -> usize {
        let mut s = self.on.stable_period().max(self.default.stable_period());
        for (_, br) in &self.cases {
            s = s.max(br.stable_period());
        }
        s - self.warm_up_period()
    }

    fn reset(&mut self) {
        self.on.reset();
        for (_, br) in self.cases.iter_mut() {
            br.reset();
        }
        self.default.reset();
        self.value = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indicators::{Identity, Sma, Value};

    #[test]
    fn first_matching_case_wins() {
        let mut ind = Match::new(
            Identity::<Real>::new(),
            vec![
                (1.0, Value::<Real>::new(10.0)),
                (2.0, Value::<Real>::new(20.0)),
                (2.0, Value::<Real>::new(999.0)), // dead code — earlier 2.0 wins
            ],
            Value::<Real>::new(0.0),
        );
        assert_eq!(ind.update(1.0), Some(10.0));
        assert_eq!(ind.update(2.0), Some(20.0));
    }

    #[test]
    fn default_fires_when_no_case_matches() {
        let mut ind = Match::new(
            Identity::<Real>::new(),
            vec![(1.0, Value::<Real>::new(10.0))],
            Value::<Real>::new(-1.0),
        );
        assert_eq!(ind.update(42.0), Some(-1.0));
    }

    #[test]
    fn on_none_propagates_none() {
        // `on` = SMA-3; for the first two bars it reads None, so the
        // match emits None regardless of branch readiness.
        let mut ind = Match::new(
            Sma::new(Identity::<Real>::new(), 3),
            vec![(10.0, Value::<Real>::new(1.0))],
            Value::<Real>::new(2.0),
        );
        assert_eq!(ind.update(10.0), None);
        assert_eq!(ind.update(10.0), None);
        // Third bar: SMA-3 settles to 10.0 → matches the case.
        assert_eq!(ind.update(10.0), Some(1.0));
    }

    #[test]
    fn non_selected_branch_still_advances() {
        // `on` alternates 1.0 / 2.0. Both branches are SMA-3s; even when
        // a branch isn't picked on a given bar, its warm-up progresses so
        // the next time it's selected it can emit `Some`.
        // All branches must share a type in the raw `Vec<(K, T)>`
        // constructor — the CLI erases through `Box<dyn DynIndicator>`
        // when composing heterogeneous chains.
        let mut ind = Match::new(
            Identity::<Real>::new(),
            vec![
                (1.0, Sma::new(Identity::<Real>::new(), 3)),
                (2.0, Sma::new(Identity::<Real>::new(), 3)),
            ],
            Sma::new(Identity::<Real>::new(), 3),
        );
        // Bars 1..=2: both branches warming. Case 1.0 fires on bar 1;
        // case 2.0 on bar 2. Both should be None (SMA-3 unsettled).
        assert_eq!(ind.update(1.0), None);
        assert_eq!(ind.update(2.0), None);
        // Bar 3: SMA-3s have all seen 3 samples via the "advance every
        // bar" rule; case 1.0 fires and reports the settled mean.
        assert_eq!(ind.update(1.0), Some((1.0 + 2.0 + 1.0) / 3.0));
    }

    #[test]
    fn warm_up_is_max_across_all_sources() {
        // All-Sma branches so the Vec's `T` is a single type (mixed
        // types require boxing — the CLI does that via DynIndicator).
        let ind = Match::new(
            Sma::new(Identity::<Real>::new(), 4), // on: warm-up 4
            vec![
                (1.0, Sma::new(Identity::<Real>::new(), 5)), // case: warm-up 5
                (2.0, Sma::new(Identity::<Real>::new(), 2)), // warm-up 2
            ],
            Sma::new(Identity::<Real>::new(), 7), // default: warm-up 7
        );
        assert_eq!(ind.warm_up_period(), 7);
    }

    #[test]
    fn reset_clears_all_sources_and_value() {
        let mut ind = Match::new(
            Identity::<Real>::new(),
            vec![(1.0, Value::<Real>::new(10.0))],
            Value::<Real>::new(0.0),
        );
        ind.update(1.0);
        assert_eq!(ind.value(), Some(10.0));
        ind.reset();
        assert_eq!(ind.value(), None);
    }

    #[test]
    fn string_pattern_dispatch() {
        // `K` isn't restricted to numbers — anything PartialEq works.
        // Here we dispatch on a string source (Identity<String>) using
        // string patterns.
        struct StrSource(String);
        impl Indicator for StrSource {
            type Input = ();
            type Output = String;
            fn update(&mut self, _: ()) -> Option<String> {
                Some(self.0.clone())
            }
            fn value(&self) -> Option<String> {
                Some(self.0.clone())
            }
            fn warm_up_period(&self) -> usize {
                0
            }
            fn reset(&mut self) {}
        }
        struct UnitVal(Real);
        impl Indicator for UnitVal {
            type Input = ();
            type Output = Real;
            fn update(&mut self, _: ()) -> Option<Real> {
                Some(self.0)
            }
            fn value(&self) -> Option<Real> {
                Some(self.0)
            }
            fn warm_up_period(&self) -> usize {
                0
            }
            fn reset(&mut self) {}
        }
        let mut ind = Match::new(
            StrSource("momentum".to_string()),
            vec![
                ("momentum".to_string(), UnitVal(2.0)),
                ("mean_rev".to_string(), UnitVal(1.0)),
            ],
            UnitVal(0.5),
        );
        assert_eq!(ind.update(()), Some(2.0));
    }
}
