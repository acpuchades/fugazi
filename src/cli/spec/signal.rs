//! YAML-deserializable [`SignalSpec`] — the boolean-signal layer.
//!
//! Split out of `spec/mod.rs`; kept in `crate::spec::signal` so paths like
//! `crate::spec::SignalSpec` still resolve via the `pub use` in `mod.rs`.

use std::sync::Arc;

use serde::Deserialize;

use fugazi::indicators::compare;
use fugazi::indicators::logic::Const;
use fugazi::indicators::{
    Book, DEFAULT_EPSILON, GetBool, IsWeekday, IsWeekend, Pick, Position, ValueStr,
};
use fugazi::prelude::*;

use super::expr::{ExprSpec, default_source};
use crate::dyn_indicator::{self, AsBool, AsReal, AsStr, DynIndicator};

/// Every atom-input leaf on the YAML side is built rooted through an
/// empty-selector `Pick::<String>` — the single-entry snapshot unpack that
/// makes existing single-series strategies keep working while multi-asset
/// strategies opt in with an explicit `!pick { symbol: ... }` selector.
fn pick_root() -> Pick<String> {
    Pick::<String>::new()
}

/// The right-hand operand of `!str_eq` / `!str_ne`.
///
/// A bare YAML string is the literal to match (`rhs: bull`) — the common case,
/// and the only shape the tag used to take. Anything else deserializes as an
/// [`ExprSpec`], so both sides of the comparison are symmetric: the same
/// constant written the long way (`rhs: !value bull`) or a second `Str` column
/// read (`rhs: !get { key: prev_regime }`) both build to a `Str`-output source.
///
/// Deserializes through a [`serde_norway::Value`] bridge rather than
/// `#[serde(untagged)]` because [`ExprSpec`] carries its own `TryFrom`
/// normalisation (bare word / tag / single-key map → tagged), which serde's
/// untagged content buffering would bypass.
#[derive(Debug, Clone, Deserialize)]
#[serde(try_from = "serde_norway::Value")]
pub enum StrOperand {
    Literal(String),
    Expr(Box<ExprSpec>),
}

impl TryFrom<serde_norway::Value> for StrOperand {
    type Error = String;

    fn try_from(v: serde_norway::Value) -> Result<Self, Self::Error> {
        match v {
            serde_norway::Value::String(s) => Ok(StrOperand::Literal(s)),
            other => ExprSpec::try_from(other).map(|e| StrOperand::Expr(Box::new(e))),
        }
    }
}

impl StrOperand {
    /// Build as a `Str`-output source. A literal materialises the same
    /// [`ValueStr`] constant the `!value <string>` expression form builds.
    fn build(
        &self,
        anchor: &Position,
        book: &Book,
        schema: &Arc<Schema>,
    ) -> Box<dyn DynIndicator> {
        match self {
            StrOperand::Literal(s) => {
                dyn_indicator::wrap(ValueStr::<fugazi::types::Snapshot<String>>::new(s.as_str()))
            }
            StrOperand::Expr(e) => e.build(anchor, book, schema),
        }
    }
}

// ---------------------------------------------------------------------------
// Boolean signals
// ---------------------------------------------------------------------------

/// A boolean condition over a candle stream — the YAML form of a `Signal`.
///
/// Deserializes via a [`serde_norway::Value`] bridge, symmetric with
/// [`ExprSpec`]'s: an incoming [`serde_norway::Value::Mapping`] with a
/// single string key (the shape a serde_json → serde_norway::Value bridge
/// produces for an externally-tagged enum) is normalised into a
/// [`serde_norway::Value::Tagged`] before deserialization proceeds. This
/// keeps `!and { lhs: !gt { ... }, rhs: !lt { ... } }` and every other
/// nesting depth working uniformly whether the top-level spec was parsed
/// via serde_norway directly (native YAML) or via the CLI's
/// `input::parse_value → serde_json::Value` normalising bridge.
#[derive(Debug, Clone, Deserialize)]
#[serde(try_from = "serde_norway::Value")]
pub enum SignalSpec {
    // --- comparisons ---
    Gt {
        lhs: Box<ExprSpec>,
        rhs: Box<ExprSpec>,
        epsilon: Option<Real>,
    },
    Lt {
        lhs: Box<ExprSpec>,
        rhs: Box<ExprSpec>,
        epsilon: Option<Real>,
    },
    Ge {
        lhs: Box<ExprSpec>,
        rhs: Box<ExprSpec>,
        epsilon: Option<Real>,
    },
    Le {
        lhs: Box<ExprSpec>,
        rhs: Box<ExprSpec>,
        epsilon: Option<Real>,
    },
    Eq {
        lhs: Box<ExprSpec>,
        rhs: Box<ExprSpec>,
        epsilon: Option<Real>,
    },
    Ne {
        lhs: Box<ExprSpec>,
        rhs: Box<ExprSpec>,
        epsilon: Option<Real>,
    },
    /// `source > level` against a constant.
    Above {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        level: Real,
    },
    /// `source < level` against a constant.
    Below {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        level: Real,
    },

    // --- crossovers ---
    CrossesAbove {
        lhs: Box<ExprSpec>,
        rhs: Box<ExprSpec>,
    },
    CrossesBelow {
        lhs: Box<ExprSpec>,
        rhs: Box<ExprSpec>,
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
    /// `lhs == rhs` on two `Str`-typed operands. `lhs` must build to a
    /// `Str`-output source (typically `!get { key: c }` where `c` is a `Str`
    /// column, or a nested string-producing expression); `rhs` is a
    /// [`StrOperand`] — a bare string literal (`rhs: bull`) or any
    /// `Str`-output expression (`rhs: !value bull`, `rhs: !get { key: d }`).
    StrEq {
        lhs: Box<ExprSpec>,
        rhs: StrOperand,
    },
    /// `lhs != rhs` on two `Str`-typed operands. The complement of
    /// [`SignalSpec::StrEq`].
    StrNe {
        lhs: Box<ExprSpec>,
        rhs: StrOperand,
    },
    /// Passthrough wrapper that reports `unstable_period() = 0`. The output
    /// and warm-up of `signal` are unchanged; the strategy-readiness gate
    /// (which counts up to `stable_period()`) no longer waits for this
    /// subtree's IIR settling tail. The explicit opt-out to the "wait for
    /// every source to be past its unstable tail" safe default; see
    /// [`fugazi::indicators::Unstable`].
    Unstable { signal: Box<SignalSpec> },
    /// A constant boolean leaf. Spelled `!value` like [`ExprSpec::Value`] —
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

/// Private mirror of [`SignalSpec`] with derived externally-tagged
/// deserialization. The public [`SignalSpec`] routes through
/// [`serde_norway::Value`] via `try_from` and normalises single-key
/// mappings into tagged values before deserializing into this mirror.
/// Kept in lock-step with the public enum variant-for-variant.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SignalSpecRaw {
    Gt {
        lhs: Box<ExprSpec>,
        rhs: Box<ExprSpec>,
        epsilon: Option<Real>,
    },
    Lt {
        lhs: Box<ExprSpec>,
        rhs: Box<ExprSpec>,
        epsilon: Option<Real>,
    },
    Ge {
        lhs: Box<ExprSpec>,
        rhs: Box<ExprSpec>,
        epsilon: Option<Real>,
    },
    Le {
        lhs: Box<ExprSpec>,
        rhs: Box<ExprSpec>,
        epsilon: Option<Real>,
    },
    Eq {
        lhs: Box<ExprSpec>,
        rhs: Box<ExprSpec>,
        epsilon: Option<Real>,
    },
    Ne {
        lhs: Box<ExprSpec>,
        rhs: Box<ExprSpec>,
        epsilon: Option<Real>,
    },
    Above {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        level: Real,
    },
    Below {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        level: Real,
    },
    CrossesAbove {
        lhs: Box<ExprSpec>,
        rhs: Box<ExprSpec>,
    },
    CrossesBelow {
        lhs: Box<ExprSpec>,
        rhs: Box<ExprSpec>,
    },
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
    All(Vec<SignalSpec>),
    Any(Vec<SignalSpec>),
    Not(Box<SignalSpec>),
    Changed(Box<SignalSpec>),
    Get { key: String },
    StrEq {
        lhs: Box<ExprSpec>,
        rhs: StrOperand,
    },
    StrNe {
        lhs: Box<ExprSpec>,
        rhs: StrOperand,
    },
    Unstable { signal: Box<SignalSpec> },
    Value(bool),
    IsWeekday,
    IsWeekend,
}

impl From<SignalSpecRaw> for SignalSpec {
    fn from(v: SignalSpecRaw) -> Self {
        match v {
            SignalSpecRaw::Gt { lhs, rhs, epsilon } => SignalSpec::Gt { lhs, rhs, epsilon },
            SignalSpecRaw::Lt { lhs, rhs, epsilon } => SignalSpec::Lt { lhs, rhs, epsilon },
            SignalSpecRaw::Ge { lhs, rhs, epsilon } => SignalSpec::Ge { lhs, rhs, epsilon },
            SignalSpecRaw::Le { lhs, rhs, epsilon } => SignalSpec::Le { lhs, rhs, epsilon },
            SignalSpecRaw::Eq { lhs, rhs, epsilon } => SignalSpec::Eq { lhs, rhs, epsilon },
            SignalSpecRaw::Ne { lhs, rhs, epsilon } => SignalSpec::Ne { lhs, rhs, epsilon },
            SignalSpecRaw::Above { source, level } => SignalSpec::Above { source, level },
            SignalSpecRaw::Below { source, level } => SignalSpec::Below { source, level },
            SignalSpecRaw::CrossesAbove { lhs, rhs } => SignalSpec::CrossesAbove { lhs, rhs },
            SignalSpecRaw::CrossesBelow { lhs, rhs } => SignalSpec::CrossesBelow { lhs, rhs },
            SignalSpecRaw::And { lhs, rhs } => SignalSpec::And { lhs, rhs },
            SignalSpecRaw::Or { lhs, rhs } => SignalSpec::Or { lhs, rhs },
            SignalSpecRaw::Xor { lhs, rhs } => SignalSpec::Xor { lhs, rhs },
            SignalSpecRaw::All(v) => SignalSpec::All(v),
            SignalSpecRaw::Any(v) => SignalSpec::Any(v),
            SignalSpecRaw::Not(inner) => SignalSpec::Not(inner),
            SignalSpecRaw::Changed(inner) => SignalSpec::Changed(inner),
            SignalSpecRaw::Get { key } => SignalSpec::Get { key },
            SignalSpecRaw::StrEq { lhs, rhs } => SignalSpec::StrEq { lhs, rhs },
            SignalSpecRaw::StrNe { lhs, rhs } => SignalSpec::StrNe { lhs, rhs },
            SignalSpecRaw::Unstable { signal } => SignalSpec::Unstable { signal },
            SignalSpecRaw::Value(b) => SignalSpec::Value(b),
            SignalSpecRaw::IsWeekday => SignalSpec::IsWeekday,
            SignalSpecRaw::IsWeekend => SignalSpec::IsWeekend,
        }
    }
}

impl TryFrom<serde_norway::Value> for SignalSpec {
    type Error = String;

    /// Normalise the incoming value into a [`serde_norway::Value::Tagged`],
    /// then deserialize into [`SignalSpecRaw`]. See the module-level doc
    /// for the shape rationale — identical to
    /// [`ExprSpec`](super::expr::ExprSpec)'s TryFrom, kept in lock-step so
    /// the two spec surfaces normalise the same way.
    fn try_from(v: serde_norway::Value) -> Result<Self, Self::Error> {
        use serde_norway::value::{Tag, TaggedValue};

        // Unit-variant tags: their content stays as `Value::Null`
        // (see the mirror `ExprSpec::TryFrom` for the "why").
        const UNIT_VARIANTS: &[&str] = &["is_weekday", "is_weekend"];

        let promote_null_for = |tag: &str, v: serde_norway::Value| match v {
            serde_norway::Value::Null if !UNIT_VARIANTS.contains(&tag) => {
                serde_norway::Value::Mapping(serde_norway::Mapping::new())
            }
            other => other,
        };

        let normalised = match v {
            serde_norway::Value::String(s) => {
                let value = promote_null_for(&s, serde_norway::Value::Null);
                serde_norway::Value::Tagged(Box::new(TaggedValue {
                    tag: Tag::new(s),
                    value,
                }))
            }
            serde_norway::Value::Tagged(tagged) => {
                let TaggedValue { tag, value } = *tagged;
                let tag_name = tag.to_string();
                let name = tag_name.strip_prefix('!').unwrap_or(&tag_name);
                let value = promote_null_for(name, value);
                serde_norway::Value::Tagged(Box::new(TaggedValue { tag, value }))
            }
            serde_norway::Value::Mapping(m) if m.len() == 1 => {
                let (k, v) = m.into_iter().next().unwrap();
                match k {
                    serde_norway::Value::String(name) => {
                        let value = promote_null_for(&name, v);
                        serde_norway::Value::Tagged(Box::new(TaggedValue {
                            tag: Tag::new(name),
                            value,
                        }))
                    }
                    other => {
                        let mut m = serde_norway::Mapping::new();
                        m.insert(other, v);
                        serde_norway::Value::Mapping(m)
                    }
                }
            }
            other => other,
        };
        let raw: SignalSpecRaw =
            serde_norway::from_value(normalised).map_err(|e| e.to_string())?;
        Ok(raw.into())
    }
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
    pub fn build(
        &self,
        anchor: &Position,
        book: &Book,
        schema: &Arc<Schema>,
    ) -> Box<dyn DynIndicator> {
        use SignalSpec::*;
        let real = |s: &ExprSpec| AsReal::new(s.build(anchor, book, schema));
        let boolean = |s: &SignalSpec| AsBool::new(s.build(anchor, book, schema));

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
                    let mut acc = AsBool::new(specs[0].build(anchor, book, schema));
                    for s in &specs[1..] {
                        let next = AsBool::new(s.build(anchor, book, schema));
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
                    let mut acc = AsBool::new(specs[0].build(anchor, book, schema));
                    for s in &specs[1..] {
                        let next = AsBool::new(s.build(anchor, book, schema));
                        acc = AsBool::new(dyn_indicator::wrap(acc.or(next)));
                    }
                    dyn_indicator::wrap(acc)
                }
            }
            Not(inner) => dyn_indicator::wrap(boolean(inner).not()),
            Changed(inner) => dyn_indicator::wrap(boolean(inner).changed()),
            Unstable { signal } => dyn_indicator::unstable_wrap(signal.build(anchor, book, schema)),
            Value(b) => {
                dyn_indicator::wrap(self::Const::<fugazi::types::Snapshot<String>>::new(*b))
            }
            Get { key } => build_signal_get(schema, key),
            StrEq { lhs, rhs } => {
                let lhs = AsStr::new(lhs.build(anchor, book, schema));
                let rhs = AsStr::new(rhs.build(anchor, book, schema));
                dyn_indicator::wrap(compare::StrEq::new(lhs, rhs))
            }
            StrNe { lhs, rhs } => {
                let lhs = AsStr::new(lhs.build(anchor, book, schema));
                let rhs = AsStr::new(rhs.build(anchor, book, schema));
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
