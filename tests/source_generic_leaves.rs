//! Integration tests for source-generic candle- and calendar-input leaves.
//!
//! Every leaf that used to hard-pin `Input = Atom` (`Field<F>`, `CurrentBar`,
//! `Calendar<F>`, `CurrentTime`, `IsWeekday`, `IsWeekend`) is now generic over
//! an atom-emitting source `S` (default [`Identity<Atom>`]). These tests prove:
//!
//! 1. The default `new()` constructor is preserved and behaves identically to
//!    the pre-refactor implementation (existing behavior + doctests already
//!    cover this; a couple of spot checks here just link the story).
//! 2. `T::of(source)` builds a functionally-equivalent leaf when the source is
//!    the identity, and reads the field through the source's output.
//! 3. Warm-up and unstable periods delegate through the source, so wrapping a
//!    leaf in a non-trivial atom source pushes the whole chain's readiness back
//!    by the source's warm-up.

use fugazi::indicator::Indicator;
use fugazi::indicators::{
    Close, Current, CurrentBar, CurrentTime, DayOfWeek, Ema, Identity, IsWeekday, IsWeekend, Year,
};
use fugazi::{Atom, Candle, Real, Timestamp};

/// A minimal atom-emitting source used to prove warm-up delegation.
///
/// Emits `None` for the first `delay` bars, then passes every subsequent atom
/// through unchanged. Its `warm_up_period() == delay + 1`, so wrapping it in a
/// leaf shifts the leaf's own warm-up by exactly that much.
#[derive(Debug, Clone)]
struct DelayedAtoms {
    delay: usize,
    seen: usize,
    latest: Option<Atom>,
}

impl DelayedAtoms {
    fn new(delay: usize) -> Self {
        Self {
            delay,
            seen: 0,
            latest: None,
        }
    }
}

impl Indicator for DelayedAtoms {
    type Input = Atom;
    type Output = Atom;

    fn update(&mut self, input: Atom) -> Option<Atom> {
        self.seen += 1;
        if self.seen > self.delay {
            self.latest = Some(input);
        } else {
            self.latest = None;
        }
        self.latest.clone()
    }

    fn value(&self) -> Option<Atom> {
        self.latest.clone()
    }

    fn warm_up_period(&self) -> usize {
        self.delay + 1
    }

    fn reset(&mut self) {
        self.seen = 0;
        self.latest = None;
    }
}

fn bar_at(ms: i64, close: Real) -> Atom {
    Atom::with_time(Candle::new(1.0, 2.0, 0.5, close, 100.0), Timestamp(ms))
}

/// Monday 2024-03-11 12:00:00 UTC.
const MONDAY_MS: i64 = 1_710_158_400_000;

#[test]
fn close_of_identity_matches_close_new() {
    let atom = bar_at(MONDAY_MS, 4.25);
    let mut lhs = Close::new();
    let mut rhs = Close::of(Identity::<Atom>::new());
    assert_eq!(lhs.update(atom.clone()), Some(4.25));
    assert_eq!(rhs.update(atom), Some(4.25));
    assert_eq!(lhs.warm_up_period(), rhs.warm_up_period());
}

#[test]
fn currentbar_of_identity_matches_currentbar_new() {
    let bar = Candle::new(1.0, 4.0, 0.5, 3.0, 1000.0);
    let atom: Atom = bar.into();
    let mut lhs = CurrentBar::new();
    let mut rhs = CurrentBar::of(Identity::<Atom>::new());
    assert_eq!(lhs.update(atom.clone()), Some(bar));
    assert_eq!(rhs.update(atom), Some(bar));
}

#[test]
fn calendar_of_identity_matches_calendar_new() {
    let atom = bar_at(MONDAY_MS, 1.0);
    let mut lhs = Year::new();
    let mut rhs = Year::of(Identity::<Atom>::new());
    assert_eq!(lhs.update(atom.clone()), Some(2024.0));
    assert_eq!(rhs.update(atom), Some(2024.0));
}

#[test]
fn currenttime_of_identity_matches_currenttime_new() {
    let atom = bar_at(MONDAY_MS, 1.0);
    let mut lhs = CurrentTime::new();
    let mut rhs = CurrentTime::of(Identity::<Atom>::new());
    assert_eq!(lhs.update(atom.clone()), Some(Timestamp(MONDAY_MS)));
    assert_eq!(rhs.update(atom), Some(Timestamp(MONDAY_MS)));
}

#[test]
fn is_weekday_of_identity_matches_is_weekday_new() {
    let atom = bar_at(MONDAY_MS, 1.0);
    let mut lhs = IsWeekday::new();
    let mut rhs = IsWeekday::of(Identity::<Atom>::new());
    assert_eq!(lhs.update(atom.clone()), Some(true));
    assert_eq!(rhs.update(atom), Some(true));
}

#[test]
fn is_weekend_of_identity_matches_is_weekend_new() {
    let atom = bar_at(MONDAY_MS, 1.0);
    let mut lhs = IsWeekend::new();
    let mut rhs = IsWeekend::of(Identity::<Atom>::new());
    assert_eq!(lhs.update(atom.clone()), Some(false));
    assert_eq!(rhs.update(atom), Some(false));
}

#[test]
fn close_over_delayed_source_shifts_warmup_and_emissions() {
    // A 2-bar delay in the source means `Close` reports warm_up_period = 3
    // (source's warm-up), and emits `None` for the first two bars.
    let mut c = Close::of(DelayedAtoms::new(2));
    assert_eq!(c.warm_up_period(), 3);
    assert_eq!(c.update(bar_at(MONDAY_MS, 10.0)), None);
    assert_eq!(c.update(bar_at(MONDAY_MS + 60_000, 11.0)), None);
    assert_eq!(c.update(bar_at(MONDAY_MS + 120_000, 12.0)), Some(12.0));
    assert_eq!(c.update(bar_at(MONDAY_MS + 180_000, 13.0)), Some(13.0));
}

#[test]
fn calendar_over_delayed_source_shifts_warmup() {
    let mut dow = DayOfWeek::of(DelayedAtoms::new(1));
    assert_eq!(dow.warm_up_period(), 2);
    assert_eq!(dow.update(bar_at(MONDAY_MS, 1.0)), None);
    assert_eq!(dow.update(bar_at(MONDAY_MS + 60_000, 1.0)), Some(1.0));
}

#[test]
fn source_generic_leaf_composes_with_an_ema() {
    // The whole point of the refactor: `Close::of(<any Atom source>)` drops
    // straight into an `Ema` (Output = Real). Prove it type-checks and runs.
    let mut ema = Ema::new(Close::of(Identity::<Atom>::new()), 3);
    let mut ema_direct = Ema::new(Current::close(), 3);
    for (i, price) in [10.0, 11.0, 12.0, 13.0, 14.0, 15.0].iter().enumerate() {
        let atom = bar_at(MONDAY_MS + i as i64 * 60_000, *price);
        assert_eq!(ema.update(atom.clone()), ema_direct.update(atom));
    }
}

#[test]
fn reset_clears_source_state_too() {
    let mut c = Close::of(DelayedAtoms::new(1));
    c.update(bar_at(MONDAY_MS, 10.0));
    c.update(bar_at(MONDAY_MS + 60_000, 11.0));
    assert_eq!(c.value(), Some(11.0));
    c.reset();
    assert_eq!(c.value(), None);
    // After reset, the source's own delay counter is back to zero — the next
    // bar is again `None` (would be `Some` if reset had not propagated).
    assert_eq!(c.update(bar_at(MONDAY_MS + 120_000, 12.0)), None);
}
