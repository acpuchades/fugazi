//! YAML-deserializable [`SignalSpec`] — the boolean-signal layer.
//!
//! Split out of `spec/mod.rs`; kept in `crate::spec::signal` so paths like
//! `crate::spec::SignalSpec` still resolve via the `pub use` in `mod.rs`.

use std::sync::Arc;

use serde::Deserialize;

use fugazi::indicators::compare;
use fugazi::indicators::logic::Const;
use fugazi::indicators::{
    Book, DEFAULT_EPSILON, Every, GetBool, IsWeekday, IsWeekend, Pick, PickAny, Position, ValueStr,
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

/// Symbol-agnostic atom root for calendar signals (`!is_weekday`,
/// `!is_weekend`) — reads only `atom.time`, which every entry in a
/// well-formed snapshot shares, so "first entry" is a stable answer even
/// when the snapshot carries multiple symbols (as in
/// [`MultiAssetStrategy`](fugazi::strategies::MultiAssetStrategy),
/// [`BasketStrategy`](fugazi::strategies::BasketStrategy), or a
/// [`Portfolio`](fugazi::portfolio::Portfolio) `rebalance_on:` gate).
/// Contrast with [`pick_root`], which panics on a 2+ entry snapshot.
fn pick_any_root() -> PickAny<String> {
    PickAny::<String>::new()
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
        portfolio_book: Option<&Book>,
        schema: &Arc<Schema>,
    ) -> Box<dyn DynIndicator> {
        match self {
            StrOperand::Literal(s) => {
                dyn_indicator::wrap(ValueStr::<fugazi::types::Snapshot<String>>::new(s.as_str()))
            }
            StrOperand::Expr(e) => e.build(anchor, book, portfolio_book, schema),
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
    /// Toggle detector with a **`Bool`-typed** inner: fires on either edge
    /// (rising OR falling). For gating on "the moment `cond` became true",
    /// use [`BecameTrue`](Self::BecameTrue); for a Real-valued transition
    /// detector (calendar rollovers, meta signals), see
    /// [`ChangedReal`](Self::ChangedReal).
    Changed(Box<SignalSpec>),
    /// Toggle detector with a **`Real`-typed** inner: fires on the single
    /// bar where the inner's Real value differs from the prior bar (any
    /// transition — including monotonic-wrap cases like `!month` going 12
    /// → 1). Same YAML tag as [`Changed`](Self::Changed) — the CLI's
    /// [`TryFrom<Value>`] tries the Bool-inner shape first and falls back
    /// to this Real-inner shape.
    ChangedReal(Box<ExprSpec>),
    /// Rising-edge detector for a Bool inner: fires the bar it transitions
    /// `false → true`. Sugar for `!and { <inner>, !changed { source: <inner>
    /// } }` bundled as one primitive so the inner doesn't need to be named
    /// twice.
    BecameTrue(Box<SignalSpec>),
    /// Falling-edge detector: mirror of [`BecameTrue`](Self::BecameTrue),
    /// fires on `true → false`.
    BecameFalse(Box<SignalSpec>),

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
    /// Sugar for `Value(false)` — reads better on a `rebalance_on` field
    /// where the intent is "never rebalance".
    Never,
    /// A periodic pulse — [`Every(N)`](crate::indicators::Every) with
    /// *delayed* first fire on bar `N-1` (0-indexed), then every `N`
    /// bars. `!every 1` fires on every bar; `!every 5` on bar 4, 9, 14, …
    /// The canonical `rebalance_on` cadence source.
    Every(usize),

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
    // Real-inner shape of `!changed`. Not exposed at the raw enum tag level
    // (the YAML tag is still `!changed`); populated by the polymorphic
    // dispatch in `SignalSpec::TryFrom<Value>` when the Bool-inner parse
    // fails and the ExprSpec fallback succeeds.
    ChangedReal(Box<ExprSpec>),
    BecameTrue(Box<SignalSpec>),
    BecameFalse(Box<SignalSpec>),
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
    Never,
    Every(usize),
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
            SignalSpecRaw::ChangedReal(inner) => SignalSpec::ChangedReal(inner),
            SignalSpecRaw::BecameTrue(inner) => SignalSpec::BecameTrue(inner),
            SignalSpecRaw::BecameFalse(inner) => SignalSpec::BecameFalse(inner),
            SignalSpecRaw::Get { key } => SignalSpec::Get { key },
            SignalSpecRaw::StrEq { lhs, rhs } => SignalSpec::StrEq { lhs, rhs },
            SignalSpecRaw::StrNe { lhs, rhs } => SignalSpec::StrNe { lhs, rhs },
            SignalSpecRaw::Unstable { signal } => SignalSpec::Unstable { signal },
            SignalSpecRaw::Value(b) => SignalSpec::Value(b),
            SignalSpecRaw::Never => SignalSpec::Never,
            SignalSpecRaw::Every(n) => SignalSpec::Every(n),
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
        const UNIT_VARIANTS: &[&str] = &[
            "is_weekday", "is_weekend", "never",
            // Wall-clock cadence sugar (all unit tags — they rewrite to
            // `!changed { source: !<calendar_accessor> }` before the raw
            // deserialize).
            "hourly", "daily", "weekly", "monthly", "quarterly", "annually",
        ];

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

        // Parse-time rewrites — sugar tags that expand to underlying
        // primitives before the raw deserialize sees them:
        //
        // * `!hourly` / `!daily` / `!weekly` / `!monthly` / `!quarterly`
        //   / `!annually`  → `!changed { source: !<hour|day|week_of_year
        //   |month|quarter|year> }` (a calendar-anchored transition
        //   detector; fires the first bar of each new hour/day/week/…).
        //
        // * `!changed <bool_or_real_inner>` polymorphic dispatch — if the
        //   inner value parses as a `SignalSpec` (Bool), use `Changed`;
        //   otherwise try `ExprSpec` (Real) and produce `ChangedReal`.
        let normalised = rewrite_cadence_sugar(normalised);
        if let Some(rewritten) = try_dispatch_edge_polymorphic(&normalised)? {
            return Ok(rewritten);
        }

        let raw: SignalSpecRaw =
            serde_norway::from_value(normalised).map_err(|e| e.to_string())?;
        Ok(raw.into())
    }
}

/// Rewrite the six wall-clock cadence sugar tags (`!hourly`, `!daily`,
/// `!weekly`, `!monthly`, `!quarterly`, `!annually`) to
/// `!changed { source: !<calendar_accessor> }` before the raw deserialize
/// runs. Kept in the parse layer so downstream debug prints show the
/// desugared form (self-documenting: readers see exactly what will run).
fn rewrite_cadence_sugar(v: serde_norway::Value) -> serde_norway::Value {
    use serde_norway::value::{Tag, TaggedValue};

    let (name, tag_value) = match &v {
        serde_norway::Value::Tagged(tv) => {
            let tag = tv.tag.to_string();
            let stripped = tag.strip_prefix('!').unwrap_or(&tag).to_string();
            (stripped, tv.value.clone())
        }
        _ => return v,
    };
    let accessor_tag = match name.as_str() {
        "hourly" => "hour",
        "daily" => "day",
        "weekly" => "week_of_year",
        "monthly" => "month",
        "quarterly" => "quarter",
        "annually" => "year",
        _ => return v,
    };
    // Ignore any payload the sugar tag came with (all unit variants).
    let _ = tag_value;

    // Build `!changed { source: !<accessor> {} }` — the source is an
    // empty-map for the calendar accessor to fall into its default-source
    // path (implicit `!pick`, same as a bare `!month` in any signal
    // subtree).
    let accessor_val = serde_norway::Value::Tagged(Box::new(TaggedValue {
        tag: Tag::new(accessor_tag),
        value: serde_norway::Value::Mapping(serde_norway::Mapping::new()),
    }));
    let mut inner_map = serde_norway::Mapping::new();
    inner_map.insert(
        serde_norway::Value::String("source".to_string()),
        accessor_val,
    );
    serde_norway::Value::Tagged(Box::new(TaggedValue {
        tag: Tag::new("changed"),
        value: serde_norway::Value::Mapping(inner_map),
    }))
}

/// Extract the inner payload of a unary edge-detector tag (`!changed`,
/// `!became_true`, `!became_false`). Accepts both YAML shapes:
///
/// * Bare tag inner: `!changed !gt { ... }` — inner value is directly the
///   sub-expression's Tagged form. Read raw.
/// * Source-mapping inner: `!changed { source: !month }` — the more
///   readable form for unit accessor tags where the bare form looks off
///   (`!changed !month` is technically fine but reads as two tags).
///
/// Returns `Some(inner)` on either shape or `None` when the outer tag
/// doesn't match `wanted`.
fn extract_edge_inner(
    v: &serde_norway::Value,
    wanted: &str,
) -> Option<serde_norway::Value> {
    let inner_payload = match v {
        serde_norway::Value::Tagged(tv)
            if tv.tag.to_string().trim_start_matches('!') == wanted =>
        {
            &tv.value
        }
        _ => return None,
    };
    // If the payload is a single-key `{ source: <inner> }` map, unwrap to
    // the inner. Otherwise return the payload as-is (the bare-tag form).
    match inner_payload {
        serde_norway::Value::Mapping(m) if m.len() == 1 => {
            match m.iter().next() {
                Some((serde_norway::Value::String(k), source)) if k == "source" => {
                    Some(source.clone())
                }
                _ => Some(inner_payload.clone()),
            }
        }
        _ => Some(inner_payload.clone()),
    }
}

/// If the incoming value is a `!changed`, `!became_true`, or
/// `!became_false` tag, dispatch its inner:
///
/// * `!changed` — try Bool ([`SignalSpec`]) first; on failure, fall back
///   to Real ([`ExprSpec`]) and produce [`SignalSpec::ChangedReal`].
/// * `!became_true` / `!became_false` — Bool inner only; wrap in the
///   corresponding rising/falling variant.
///
/// Returns `Ok(Some(spec))` on match, `Ok(None)` on non-match, and `Err`
/// when both fallback parses fail (Bool-side error surfaced).
fn try_dispatch_edge_polymorphic(
    v: &serde_norway::Value,
) -> Result<Option<SignalSpec>, String> {
    // `!changed` — Bool-first, Real-fallback.
    if let Some(inner) = extract_edge_inner(v, "changed") {
        return match SignalSpec::try_from(inner.clone()) {
            Ok(bool_inner) => Ok(Some(SignalSpec::Changed(Box::new(bool_inner)))),
            Err(bool_err) => match ExprSpec::try_from(inner) {
                Ok(real_inner) => Ok(Some(SignalSpec::ChangedReal(Box::new(real_inner)))),
                Err(_) => Err(bool_err),
            },
        };
    }
    // `!became_true` — Bool inner only.
    if let Some(inner) = extract_edge_inner(v, "became_true") {
        let bool_inner = SignalSpec::try_from(inner)?;
        return Ok(Some(SignalSpec::BecameTrue(Box::new(bool_inner))));
    }
    // `!became_false` — Bool inner only.
    if let Some(inner) = extract_edge_inner(v, "became_false") {
        let bool_inner = SignalSpec::try_from(inner)?;
        return Ok(Some(SignalSpec::BecameFalse(Box::new(bool_inner))));
    }
    Ok(None)
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
        portfolio_book: Option<&Book>,
        schema: &Arc<Schema>,
    ) -> Box<dyn DynIndicator> {
        use SignalSpec::*;
        let real = |s: &ExprSpec| AsReal::new(s.build(anchor, book, portfolio_book, schema));
        let boolean =
            |s: &SignalSpec| AsBool::new(s.build(anchor, book, portfolio_book, schema));

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
                    let mut acc =
                        AsBool::new(specs[0].build(anchor, book, portfolio_book, schema));
                    for s in &specs[1..] {
                        let next = AsBool::new(s.build(anchor, book, portfolio_book, schema));
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
                    let mut acc =
                        AsBool::new(specs[0].build(anchor, book, portfolio_book, schema));
                    for s in &specs[1..] {
                        let next = AsBool::new(s.build(anchor, book, portfolio_book, schema));
                        acc = AsBool::new(dyn_indicator::wrap(acc.or(next)));
                    }
                    dyn_indicator::wrap(acc)
                }
            }
            Not(inner) => dyn_indicator::wrap(boolean(inner).not()),
            Changed(inner) => dyn_indicator::wrap(boolean(inner).changed()),
            ChangedReal(inner) => dyn_indicator::wrap(real(inner).changed()),
            BecameTrue(inner) => dyn_indicator::wrap(boolean(inner).became_true()),
            BecameFalse(inner) => dyn_indicator::wrap(boolean(inner).became_false()),
            Unstable { signal } => {
                dyn_indicator::unstable_wrap(signal.build(anchor, book, portfolio_book, schema))
            }
            Value(b) => {
                dyn_indicator::wrap(self::Const::<fugazi::types::Snapshot<String>>::new(*b))
            }
            Never => {
                dyn_indicator::wrap(self::Const::<fugazi::types::Snapshot<String>>::new(false))
            }
            SignalSpec::Every(n) => {
                dyn_indicator::wrap(self::Every::<fugazi::types::Snapshot<String>>::new(*n))
            }
            Get { key } => build_signal_get(schema, key),
            StrEq { lhs, rhs } => {
                let lhs = AsStr::new(lhs.build(anchor, book, portfolio_book, schema));
                let rhs = AsStr::new(rhs.build(anchor, book, portfolio_book, schema));
                dyn_indicator::wrap(compare::StrEq::new(lhs, rhs))
            }
            StrNe { lhs, rhs } => {
                let lhs = AsStr::new(lhs.build(anchor, book, portfolio_book, schema));
                let rhs = AsStr::new(rhs.build(anchor, book, portfolio_book, schema));
                dyn_indicator::wrap(compare::StrNe::new(lhs, rhs))
            }

            IsWeekday => dyn_indicator::wrap(self::IsWeekday::of(pick_any_root())),
            IsWeekend => dyn_indicator::wrap(self::IsWeekend::of(pick_any_root())),
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
