//! YAML-deserializable [`SignalSpec`] — the boolean-signal layer.
//!
//! Split out of `spec/mod.rs`; kept in `crate::spec::signal` so paths like
//! `crate::spec::SignalSpec` still resolve via the `pub use` in `mod.rs`.

use std::sync::Arc;

use serde::Deserialize;

use fugazi::indicators::compare;
use fugazi::indicators::logic::Const;
use fugazi::indicators::{
    DEFAULT_EPSILON, GetBool, IsWeekday, IsWeekend, Pick, Position, ValueStr,
};
use fugazi::prelude::*;

use super::source::{SourceSpec, default_source};
use crate::dyn_indicator::{self, AsBool, AsReal, AsStr, DynIndicator};

/// Every atom-input leaf on the YAML side is built rooted through an
/// empty-selector `Pick::<String>` — the single-entry snapshot unpack that
/// makes existing single-series strategies keep working while multi-asset
/// strategies opt in with an explicit `!pick { symbol: ... }` selector.
fn pick_root() -> Pick<String> {
    Pick::<String>::new()
}

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

    /// Read a `Bool` overlay column as a signal. The column's declared type
    /// in the atom stream's schema must be `Bool`; a `Real` or `Str` column
    /// with the same name is a build-time error, since only a bool value
    /// can act as a signal directly. For a `Str` column, wrap in
    /// [`SignalSpec::StrEq`] against a constant; for a `Real` column, use a
    /// comparison from the `!gt` / `!lt` / etc. family with a `!get` in the
    /// source position.
    Get {
        key: String,
    },
    /// `lhs == rhs` on a `Str`-typed source and a string literal. `lhs` must
    /// build to a `Str`-output source (typically `!get { key: c }` where `c`
    /// is a `Str` column, or a nested string-producing expression); `rhs` is
    /// the string literal to match against.
    StrEq {
        lhs: Box<SourceSpec>,
        rhs: String,
    },
    /// `lhs != rhs` on a `Str`-typed source and a string literal. The
    /// complement of [`SignalSpec::StrEq`].
    StrNe {
        lhs: Box<SourceSpec>,
        rhs: String,
    },
    /// Passthrough wrapper that reports `unstable_period() = 0`. The output
    /// and warm-up of `signal` are unchanged; the strategy-readiness gate
    /// (which counts up to `stable_period()`) no longer waits for this
    /// subtree's IIR settling tail. The explicit opt-out to the "wait for
    /// every source to be past its unstable tail" safe default; see
    /// [`fugazi::indicators::Unstable`].
    Unstable { signal: Box<SignalSpec> },
    /// A constant boolean leaf. Spelled `!value` like [`SourceSpec::Value`] —
    /// one tag for "a literal", typed by position (bool here, number there).
    Value(bool),

    // --- calendar signals (read `atom.time`, emit bool; None when time is
    // absent). Anything else (`is_monday`, "hour < 9", "trading window") is a
    // composition against the numeric calendar sources: e.g. `!eq { lhs:
    // !day_of_week, rhs: !value 1 }` for Monday.
    /// True on Monday through Friday, false on Sat/Sun. `None` when
    /// `atom.time` is absent.
    IsWeekday,
    /// True on Sat/Sun, false on Mon–Fri. `None` when `atom.time` is absent.
    IsWeekend,
}

/// Resolve an optional tolerance to its concrete value.
fn eps(epsilon: &Option<Real>) -> Real {
    epsilon.unwrap_or(DEFAULT_EPSILON)
}

impl SignalSpec {
    /// Construct the live, runtime-typed signal this spec describes as a
    /// `Box<dyn DynIndicator>` with `output_type() == DynType::Bool`. `anchor`
    /// is threaded to any `entry` / `peak` / `trough` source leaf; `schema` is
    /// the overlay [`Schema`] the atom stream carries, used by `!get`-shaped
    /// leaves for type-directed dispatch.
    pub fn build(&self, anchor: &Position, schema: &Arc<Schema>) -> Box<dyn DynIndicator> {
        use SignalSpec::*;
        let real = |s: &SourceSpec| AsReal::new(s.build(anchor, schema));
        let boolean = |s: &SignalSpec| AsBool::new(s.build(anchor, schema));

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
                    dyn_indicator::wrap(self::Const::<fugazi::types::Snapshot<String>>::new(true))
                } else {
                    let mut acc = AsBool::new(specs[0].build(anchor, schema));
                    for s in &specs[1..] {
                        let next = AsBool::new(s.build(anchor, schema));
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
                    dyn_indicator::wrap(self::Const::<fugazi::types::Snapshot<String>>::new(false))
                } else {
                    let mut acc = AsBool::new(specs[0].build(anchor, schema));
                    for s in &specs[1..] {
                        let next = AsBool::new(s.build(anchor, schema));
                        acc = AsBool::new(dyn_indicator::wrap(acc.or(next)));
                    }
                    dyn_indicator::wrap(acc)
                }
            }
            Not(inner) => dyn_indicator::wrap(boolean(inner).not()),
            Changed(inner) => dyn_indicator::wrap(boolean(inner).changed()),
            Unstable { signal } => dyn_indicator::unstable_wrap(signal.build(anchor, schema)),
            Value(b) => {
                dyn_indicator::wrap(self::Const::<fugazi::types::Snapshot<String>>::new(*b))
            }
            Get { key } => build_signal_get(schema, key),
            StrEq { lhs, rhs } => {
                let lhs = AsStr::new(lhs.build(anchor, schema));
                let rhs: ValueStr<fugazi::types::Snapshot<String>> = ValueStr::new(rhs.as_str());
                dyn_indicator::wrap(compare::StrEq::new(lhs, rhs))
            }
            StrNe { lhs, rhs } => {
                let lhs = AsStr::new(lhs.build(anchor, schema));
                let rhs: ValueStr<fugazi::types::Snapshot<String>> = ValueStr::new(rhs.as_str());
                dyn_indicator::wrap(compare::StrNe::new(lhs, rhs))
            }

            IsWeekday => dyn_indicator::wrap(self::IsWeekday::of(pick_root())),
            IsWeekend => dyn_indicator::wrap(self::IsWeekend::of(pick_root())),
        }
    }
}

/// Build the signal-side `!get { key }` variant: only a `Bool` column is a
/// valid signal source. A `Real` column can't stand alone as a signal (it's
/// numeric — pair it with a comparison in a `!gt` / `!lt` position instead);
/// a `Str` column likewise can't (wrap it in `!str_eq { lhs: !get { key }, rhs:
/// "value" }`). Missing keys produce the same registered-keys-listing message
/// as the source-side `!get`.
fn build_signal_get(schema: &Arc<Schema>, key: &str) -> Box<dyn DynIndicator> {
    match schema.type_of_key(key) {
        Some(OverlayType::Bool) => dyn_indicator::wrap(GetBool::of(schema, key, pick_root())),
        Some(OverlayType::Real) => panic!(
            "!get {{ key: {key:?} }} in signal position: column is Real, but a signal must be \
             Bool. Use a comparison like `!gt {{ lhs: !get {{ key: {key:?} }}, rhs: ... }}` \
             instead.",
        ),
        Some(OverlayType::Str) => panic!(
            "!get {{ key: {key:?} }} in signal position: column is Str, but a signal must be \
             Bool. Wrap it in `!str_eq {{ lhs: !get {{ key: {key:?} }}, rhs: \"value\" }}` \
             (or `!str_ne`) instead.",
        ),
        None => {
            let registered: Vec<&str> = schema.keys().collect();
            if registered.is_empty() {
                panic!(
                    "!get {{ key: {key:?} }} in signal position: no overlay side channel is \
                     bound — feed `--series` data that carries additional (non-OHLCV) columns \
                     to attach overlays",
                );
            } else {
                panic!(
                    "!get {{ key: {key:?} }} in signal position: overlay column not registered. \
                     Registered columns: {}",
                    registered.join(", "),
                );
            }
        }
    }
}
