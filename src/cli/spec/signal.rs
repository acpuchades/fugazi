//! YAML-deserializable [`SignalSpec`] — the boolean-signal layer.
//!
//! Split out of `spec/mod.rs`; kept in `crate::spec::signal` so paths like
//! `crate::spec::SignalSpec` still resolve via the `pub use` in `mod.rs`.

use serde::Deserialize;

use fugazi::indicators::compare;
use fugazi::indicators::logic::Const;
use fugazi::indicators::{DEFAULT_EPSILON, Position};
use fugazi::prelude::*;

use super::source::{SourceSpec, default_source};
use crate::dyn_indicator::{self, AsBool, AsReal, DynIndicator};

// ---------------------------------------------------------------------------
// Boolean signals
// ---------------------------------------------------------------------------

/// A boolean condition over a candle stream — the YAML form of a `Signal`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalSpec {
    // --- comparisons ---
    Gt {
        lhs: Box<SourceSpec>,
        rhs: Box<SourceSpec>,
        epsilon: Option<Real>,
    },
    Lt {
        lhs: Box<SourceSpec>,
        rhs: Box<SourceSpec>,
        epsilon: Option<Real>,
    },
    Ge {
        lhs: Box<SourceSpec>,
        rhs: Box<SourceSpec>,
        epsilon: Option<Real>,
    },
    Le {
        lhs: Box<SourceSpec>,
        rhs: Box<SourceSpec>,
        epsilon: Option<Real>,
    },
    Eq {
        lhs: Box<SourceSpec>,
        rhs: Box<SourceSpec>,
        epsilon: Option<Real>,
    },
    Ne {
        lhs: Box<SourceSpec>,
        rhs: Box<SourceSpec>,
        epsilon: Option<Real>,
    },
    /// `source > level` against a constant.
    Above {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        level: Real,
    },
    /// `source < level` against a constant.
    Below {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        level: Real,
    },

    // --- crossovers ---
    CrossesAbove {
        lhs: Box<SourceSpec>,
        rhs: Box<SourceSpec>,
    },
    CrossesBelow {
        lhs: Box<SourceSpec>,
        rhs: Box<SourceSpec>,
    },

    // --- boolean logic ---
    And {
        lhs: Box<SignalSpec>,
        rhs: Box<SignalSpec>,
    },
    Or {
        lhs: Box<SignalSpec>,
        rhs: Box<SignalSpec>,
    },
    Xor {
        lhs: Box<SignalSpec>,
        rhs: Box<SignalSpec>,
    },
    /// AND-fold of a list (empty ⇒ constant `true`).
    All(Vec<SignalSpec>),
    /// OR-fold of a list (empty ⇒ constant `false`).
    Any(Vec<SignalSpec>),
    Not(Box<SignalSpec>),
    Changed(Box<SignalSpec>),
    /// Reports whether `signal`'s chain has been fed at least its
    /// `stable_period()` samples. Compose in an `!and` with an entry signal to
    /// gate the entry on stability (see
    /// [`fugazi::indicators::Stable`]).
    Stable { signal: Box<SignalSpec> },
    /// A constant boolean leaf. Spelled `!value` like [`SourceSpec::Value`] —
    /// one tag for "a literal", typed by position (bool here, number there).
    Value(bool),
}

/// Resolve an optional tolerance to its concrete value.
fn eps(epsilon: &Option<Real>) -> Real {
    epsilon.unwrap_or(DEFAULT_EPSILON)
}

impl SignalSpec {
    /// Construct the live, runtime-typed signal this spec describes as a
    /// `Box<dyn DynIndicator>` with `output_type() == DynType::Bool`. `anchor`
    /// is threaded to any `entry` / `peak` / `trough` source leaf.
    pub fn build(&self, anchor: &Position) -> Box<dyn DynIndicator> {
        use SignalSpec::*;
        let real = |s: &SourceSpec| AsReal::new(s.build(anchor));
        let boolean = |s: &SignalSpec| AsBool::new(s.build(anchor));

        match self {
            Gt { lhs, rhs, epsilon } => dyn_indicator::wrap(compare::Gt::with_epsilon(
                real(lhs),
                real(rhs),
                eps(epsilon),
            )),
            Lt { lhs, rhs, epsilon } => dyn_indicator::wrap(compare::Lt::with_epsilon(
                real(lhs),
                real(rhs),
                eps(epsilon),
            )),
            Ge { lhs, rhs, epsilon } => dyn_indicator::wrap(compare::Ge::with_epsilon(
                real(lhs),
                real(rhs),
                eps(epsilon),
            )),
            Le { lhs, rhs, epsilon } => dyn_indicator::wrap(compare::Le::with_epsilon(
                real(lhs),
                real(rhs),
                eps(epsilon),
            )),
            Eq { lhs, rhs, epsilon } => dyn_indicator::wrap(compare::Eq::with_epsilon(
                real(lhs),
                real(rhs),
                eps(epsilon),
            )),
            Ne { lhs, rhs, epsilon } => dyn_indicator::wrap(compare::Ne::with_epsilon(
                real(lhs),
                real(rhs),
                eps(epsilon),
            )),
            Above { source, level } => dyn_indicator::wrap(real(source).above(*level)),
            Below { source, level } => dyn_indicator::wrap(real(source).below(*level)),

            // A crossover clones its operands (the `Change` half needs a fresh
            // comparison state); rebuild each operand from the spec so we get
            // two independently-advanced instances.
            CrossesAbove { lhs, rhs } => {
                let cmp = || real(lhs).gt(real(rhs));
                dyn_indicator::wrap(cmp().and(cmp().changed()))
            }
            CrossesBelow { lhs, rhs } => {
                let cmp = || real(lhs).lt(real(rhs));
                dyn_indicator::wrap(cmp().and(cmp().changed()))
            }

            And { lhs, rhs } => dyn_indicator::wrap(boolean(lhs).and(boolean(rhs))),
            Or { lhs, rhs } => dyn_indicator::wrap(boolean(lhs).or(boolean(rhs))),
            Xor { lhs, rhs } => dyn_indicator::wrap(boolean(lhs).xor(boolean(rhs))),
            All(specs) => {
                if specs.is_empty() {
                    dyn_indicator::wrap(self::Const::<Atom>::new(true))
                } else {
                    let mut acc = AsBool::new(specs[0].build(anchor));
                    for s in &specs[1..] {
                        let next = AsBool::new(s.build(anchor));
                        // AsBool `and` AsBool → concrete Combine; wrap in AsBool
                        // by round-tripping through the box so the fold's accumulator
                        // stays a single library type.
                        acc = AsBool::new(dyn_indicator::wrap(acc.and(next)));
                    }
                    dyn_indicator::wrap(acc)
                }
            }
            Any(specs) => {
                if specs.is_empty() {
                    dyn_indicator::wrap(self::Const::<Atom>::new(false))
                } else {
                    let mut acc = AsBool::new(specs[0].build(anchor));
                    for s in &specs[1..] {
                        let next = AsBool::new(s.build(anchor));
                        acc = AsBool::new(dyn_indicator::wrap(acc.or(next)));
                    }
                    dyn_indicator::wrap(acc)
                }
            }
            Not(inner) => dyn_indicator::wrap(boolean(inner).not()),
            Changed(inner) => dyn_indicator::wrap(boolean(inner).changed()),
            Stable { signal } => dyn_indicator::stable_check(signal.build(anchor).stable_period()),
            Value(b) => dyn_indicator::wrap(self::Const::<Atom>::new(*b)),
        }
    }
}
