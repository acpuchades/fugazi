//! Static type checking for [`NodeSpec`] trees, for `fugazi check`.
//!
//! Every tag declares what its children must *produce* and what it produces in
//! turn — `!sma` needs a `Real` source, `!atr` a `Candle` one, `!close` an
//! `Atom`. Those constraints are enforced at build time by the `AsReal` /
//! `AsCandle` / `AsAtom` views, whose mismatch is an `assert_eq!`: a run dies
//! mid-build with `left: Str, right: Real` and no indication of *where*. Since
//! the constraints are a property of the tags rather than of the data, they can
//! be decided statically, which is what this module does.
//!
//! ## What it cannot decide
//!
//! [`output_type`] returns `Option<PayloadType>`, and `None` means **unknown —
//! skip**, never "invalid". Three things are genuinely undecidable without
//! data or a driver:
//!
//! * `!get { key }` resolves its type against the overlay [`Schema`](crate::Schema), which
//!   `check` has no access to (there is no `--series` at check time). Any
//!   expression rooted in a `!get` is therefore unchecked.
//! * A `!param` / `!arg` placeholder standing in for a whole expression parses
//!   as a hole and could be anything.
//! * `!unstable` and `!resample` are passthroughs, so they are exactly as known
//!   as what they wrap.
//!
//! Skipping is always sound here: an unknown child never *fails* a check, so
//! the pass has no false positives — only the mismatches it can prove.
//!
//! ## Drift
//!
//! This duplicates knowledge that also lives in the [`NodeSpec::build`] arms,
//! so the two could drift apart and this one could start lying. Both matches
//! below are **exhaustive with no wildcard arm**, so adding a variant to
//! `NodeSpec` fails to compile until it is classified here — the compiler is
//! the drift guard, not a test that might not be run. The tests at the bottom
//! then pin the classifications that exist against what `build` actually
//! produces.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use crate::runtime::PayloadType;
use crate::spec::expr::{NodeSpec, ValueLit};
use crate::spec::grammar::GrammarTag;

/// What a child slot is allowed to produce.
#[derive(Debug, Clone, Copy)]
enum Expect {
    /// Exactly this type.
    Only(PayloadType),
    /// Any of these — `!match`'s `on:` dispatches on a number *or* a string.
    OneOf(&'static [PayloadType]),
}

impl Expect {
    fn admits(self, ty: PayloadType) -> bool {
        match self {
            Expect::Only(t) => t == ty,
            Expect::OneOf(ts) => ts.contains(&ty),
        }
    }

    fn describe(self) -> String {
        match self {
            Expect::Only(t) => t.to_string(),
            Expect::OneOf(ts) => ts
                .iter()
                .map(|t| t.to_string())
                .collect::<Vec<_>>()
                .join(" or "),
        }
    }

    /// The admitted types as a list. Empty for `OneOf(&[])` — a passthrough
    /// slot that constrains nothing (see [`slot_demand`]).
    fn types(self) -> Vec<PayloadType> {
        match self {
            Expect::Only(t) => vec![t],
            Expect::OneOf(ts) => ts.to_vec(),
        }
    }
}

const REAL: Expect = Expect::Only(PayloadType::Real);
const CANDLE: Expect = Expect::Only(PayloadType::Candle);
const ATOM: Expect = Expect::Only(PayloadType::Atom);
const BOOL: Expect = Expect::Only(PayloadType::Bool);
/// `!changed` accepts either a Bool inner (toggle) or a Real inner (any-change).
const BOOL_OR_REAL: Expect = Expect::OneOf(&[PayloadType::Bool, PayloadType::Real]);
/// A string comparison's `lhs` reads a `Str` column.
const STR: Expect = Expect::Only(PayloadType::Str);
/// `!match`'s `on:` — numeric or string dispatch (the two are not mixable
/// within one `!match`, but that is a build-time check against the cases).
const REAL_OR_STR: Expect = Expect::OneOf(&[PayloadType::Real, PayloadType::Str]);

/// What this expression produces, or `None` when that cannot be known
/// statically (see the module docs — `None` means *skip*, never *invalid*).
pub fn output_type(spec: &NodeSpec) -> Option<PayloadType> {
    use NodeSpec::*;
    match spec {
        // --- the non-Real leaves ---
        Current { .. } => Some(PayloadType::Candle),
        Pick { .. } => Some(PayloadType::Atom),
        Time { .. } => Some(PayloadType::Time),
        Value(ValueLit::Str(_)) => Some(PayloadType::Str),
        Value(ValueLit::Real(_)) => Some(PayloadType::Real),
        Value(ValueLit::Bool(_)) => Some(PayloadType::Bool),
        // A list literal is rewritten to a scalar per child before any build
        // reaches it; outside a portfolio weight template it is invalid, which
        // `build` reports. Nothing useful to say about its type here.
        Value(ValueLit::List(_)) => None,
        // Build-time source selectors, not values. `build` panics if one is
        // used as an expression; that is its diagnostic to give, not ours.
        StrategyBook | PortfolioBook => None,
        // Schema-dependent: the column's declared type decides.
        Get { .. } => None,

        // --- passthroughs: exactly as known as what they wrap ---
        Unstable { source } => output_type(source),
        Resample { inner, .. } => output_type(inner),

        // --- absorbed boolean signals: all produce Bool ---
        Gt { .. }
        | Lt { .. }
        | Ge { .. }
        | Le { .. }
        | Eq { .. }
        | Ne { .. }
        | Above { .. }
        | Below { .. }
        | CrossesAbove { .. }
        | CrossesBelow { .. }
        | And { .. }
        | Or { .. }
        | Xor { .. }
        | All(_)
        | Any(_)
        | Not(_)
        | Changed(_)
        | BecameTrue(_)
        | BecameFalse(_)
        | StrEq { .. }
        | StrNe { .. }
        | Never
        | Every(_)
        | IsWeekday
        | IsWeekend
        | HasColumn { .. } => Some(PayloadType::Bool),

        // --- everything else is Real ---
        Close { .. }
        | High { .. }
        | Low { .. }
        | Open { .. }
        | Volume { .. }
        | Typical { .. }
        | Median { .. }
        | Entry
        | Peak
        | Trough
        | Equity { .. }
        | EquityPeak { .. }
        | Drawdown { .. }
        | ReturnPerBar { .. }
        | TradePnl { .. }
        | TradeReturn { .. }
        | Ema { .. }
        | Sma { .. }
        | Rma { .. }
        | Wma { .. }
        | Hma { .. }
        | Rsi { .. }
        | StdDev { .. }
        | Skewness { .. }
        | Kurtosis { .. }
        | ZScore { .. }
        | Percentile { .. }
        | PercentileRank { .. }
        | BarsSince { .. }
        | BarsSinceHigh { .. }
        | BarsSinceLow { .. }
        | Correlation { .. }
        | Covariance { .. }
        | Beta { .. }
        | LinregSlope { .. }
        | LinregIntercept { .. }
        | LinregValue { .. }
        | LinregR2 { .. }
        | Pow { .. }
        | Max { .. }
        | Min { .. }
        | Clamp { .. }
        | Abs { .. }
        | Sign { .. }
        | Sqrt { .. }
        | Tanh { .. }
        | Sigmoid { .. }
        | CumSum { .. }
        | CumMax { .. }
        | CumMin { .. }
        | VarianceRatio { .. }
        | Cci { .. }
        | Stochastic { .. }
        | StochRsi { .. }
        | MacdLine { .. }
        | MacdSignal { .. }
        | MacdHistogram { .. }
        | BbUpper { .. }
        | BbMiddle { .. }
        | BbLower { .. }
        | KeltnerUpper { .. }
        | KeltnerMiddle { .. }
        | KeltnerLower { .. }
        | DonchianUpper { .. }
        | DonchianMiddle { .. }
        | DonchianLower { .. }
        | Adx { .. }
        | PlusDi { .. }
        | MinusDi { .. }
        | DmiPlusDi { .. }
        | DmiMinusDi { .. }
        | AroonUp { .. }
        | AroonDown { .. }
        | AroonOscillator { .. }
        | Atr { .. }
        | Parkinson { .. }
        | GarmanKlass { .. }
        | RogersSatchell { .. }
        | Mfi { .. }
        | WilliamsR { .. }
        | Obv { .. }
        | Vwap { .. }
        | Ad { .. }
        | TrueRange { .. }
        | Sar { .. }
        | VolTarget { .. }
        | AtrRisk { .. }
        | DrawdownThrottle { .. }
        | EquityVolTarget { .. }
        | FractionalKelly { .. }
        | Sharpe { .. }
        | Sortino { .. }
        | Volatility { .. }
        | MaxDrawdown { .. }
        | Calmar { .. }
        | Add { .. }
        | Sub { .. }
        | Mul { .. }
        | Div { .. }
        | IfElse { .. }
        | Match { .. }
        | Lag { .. }
        | Diff { .. }
        | Ratio { .. }
        | Roc { .. }
        | RollingMax { .. }
        | RollingMin { .. }
        | Log { .. }
        | Exp { .. }
        | Latch { .. }
        | Year { .. }
        | Month { .. }
        | Day { .. }
        | Hour { .. }
        | Minute { .. }
        | Second { .. }
        | DayOfWeek { .. }
        | DayOfYear { .. }
        | WeekOfYear { .. }
        | Quarter { .. }
        | UnixSeconds { .. }
        | UnixMillis { .. } => Some(PayloadType::Real),
    }
}

/// This node's directly-typed children, as `(slot label, expected, child)`.
///
/// Only slots whose type `build` constrains appear. A boolean-output child
/// (`!bars_since`'s source, `!if_else`'s `cond`) is always `Bool` by
/// construction, so there is nothing to check.
fn children(spec: &NodeSpec) -> Vec<(&'static str, Expect, &NodeSpec)> {
    use NodeSpec::*;
    // A defaulted `source:` that is absent needs no check — the default root is
    // well-typed by construction.
    fn opt<'a>(
        label: &'static str,
        e: Expect,
        s: &'a Option<Box<NodeSpec>>,
    ) -> Vec<(&'static str, Expect, &'a NodeSpec)> {
        s.as_deref().map(|c| (label, e, c)).into_iter().collect()
    }

    match spec {
        // --- Real-source families ---
        Ema { source, .. }
        | Sma { source, .. }
        | Rma { source, .. }
        | Wma { source, .. }
        | Hma { source, .. }
        | Rsi { source, .. }
        | StdDev { source, .. }
        | Skewness { source, .. }
        | Kurtosis { source, .. }
        | ZScore { source, .. }
        | Percentile { source, .. }
        | PercentileRank { source, .. }
        | BarsSinceHigh { source, .. }
        | BarsSinceLow { source, .. }
        | VarianceRatio { source, .. }
        | Cci { source, .. }
        | Stochastic { source, .. }
        | StochRsi { source, .. }
        | MacdLine { source, .. }
        | MacdSignal { source, .. }
        | MacdHistogram { source, .. }
        | BbUpper { source, .. }
        | BbMiddle { source, .. }
        | BbLower { source, .. }
        | Lag { source, .. }
        | Diff { source, .. }
        | Ratio { source, .. }
        | Roc { source, .. }
        | RollingMax { source, .. }
        | RollingMin { source, .. }
        | Log { source, .. }
        | Exp { source, .. }
        | Abs { source }
        | Sign { source }
        | Sqrt { source }
        | Tanh { source }
        | Sigmoid { source }
        | CumSum { source }
        | CumMax { source }
        | CumMin { source }
        | LinregSlope { source, .. }
        | LinregIntercept { source, .. }
        | LinregValue { source, .. }
        | LinregR2 { source, .. }
        | Latch { source } => vec![("source", REAL, source)],

        // --- Candle-source families (bar indicators) ---
        Adx { source, .. }
        | PlusDi { source, .. }
        | MinusDi { source, .. }
        | DmiPlusDi { source, .. }
        | DmiMinusDi { source, .. }
        | AroonUp { source, .. }
        | AroonDown { source, .. }
        | AroonOscillator { source, .. }
        | Atr { source, .. }
        | Parkinson { source, .. }
        | GarmanKlass { source, .. }
        | RogersSatchell { source, .. }
        | Mfi { source, .. }
        | WilliamsR { source, .. }
        | Obv { source }
        | Vwap { source, .. }
        | Ad { source }
        | TrueRange { source }
        | Sar { source, .. } => vec![("source", CANDLE, source)],

        // --- Atom-source leaves (candle fields, calendar, overlay readers) ---
        Close { source }
        | High { source }
        | Low { source }
        | Open { source }
        | Volume { source }
        | Typical { source }
        | Median { source }
        | Current { source }
        | Get { source, .. }
        | Year { source }
        | Month { source }
        | Day { source }
        | Hour { source }
        | Minute { source }
        | Second { source }
        | DayOfWeek { source }
        | DayOfYear { source }
        | WeekOfYear { source }
        | Quarter { source }
        | UnixSeconds { source }
        | UnixMillis { source }
        | Time { source } => opt("source", ATOM, source),

        // Price-reading sizing recipes root on an atom source too.
        VolTarget { source, .. } | AtrRisk { source, .. } => opt("source", ATOM, source),

        // --- two-operand Real ---
        Correlation { lhs, rhs, .. }
        | Covariance { lhs, rhs, .. }
        | Beta { lhs, rhs, .. }
        | Add { lhs, rhs }
        | Sub { lhs, rhs }
        | Mul { lhs, rhs }
        | Div { lhs, rhs }
        | Pow { lhs, rhs }
        | Max { lhs, rhs }
        | Min { lhs, rhs } => vec![("lhs", REAL, lhs), ("rhs", REAL, rhs)],

        // Three Real slots: the value and the band it is held inside.
        Clamp {
            source,
            lower,
            upper,
        } => vec![
            ("source", REAL, source),
            ("lower", REAL, lower),
            ("upper", REAL, upper),
        ],

        DonchianUpper { high, low, .. }
        | DonchianMiddle { high, low, .. }
        | DonchianLower { high, low, .. } => vec![("high", REAL, high), ("low", REAL, low)],

        KeltnerUpper {
            source,
            candle_source,
            ..
        }
        | KeltnerMiddle {
            source,
            candle_source,
            ..
        }
        | KeltnerLower {
            source,
            candle_source,
            ..
        } => vec![
            ("source", REAL, source),
            ("candle_source", CANDLE, candle_source),
        ],

        // `inner` is *chained* onto the resampled candle stream, so what `build`
        // constrains is its **input** (must accept a Candle), not its output —
        // which is why `output_type` reads through to it. No output demand.
        Resample { inner, source, .. } => vec![
            ("source", CANDLE, source),
            ("inner", Expect::OneOf(&[]), inner),
        ],

        IfElse {
            cond,
            then,
            otherwise,
        } => vec![
            ("cond", BOOL, cond),
            ("then", REAL, then),
            ("otherwise", REAL, otherwise),
        ],

        Match { on, cases, default } => {
            let mut out = vec![("on", REAL_OR_STR, &**on), ("default", REAL, &**default)];
            for case in cases {
                out.push(("case value", REAL, &case.value));
            }
            out
        }

        // Passthrough: its own child carries whatever type; nothing to demand.
        Unstable { source } => vec![("source", Expect::OneOf(&[]), source)],

        // --- absorbed boolean signals ---
        // `!eq` / `!ne` dispatch on the lhs and admit Real or Str on both
        // sides (a Real-vs-Str *pairing* is still rejected at build).
        Eq { lhs, rhs, .. } | Ne { lhs, rhs, .. } => {
            vec![("lhs", REAL_OR_STR, lhs), ("rhs", REAL_OR_STR, rhs)]
        }
        Gt { lhs, rhs, .. }
        | Lt { lhs, rhs, .. }
        | Ge { lhs, rhs, .. }
        | Le { lhs, rhs, .. }
        | CrossesAbove { lhs, rhs }
        | CrossesBelow { lhs, rhs } => vec![("lhs", REAL, lhs), ("rhs", REAL, rhs)],
        Above { source, .. } | Below { source, .. } => vec![("source", REAL, source)],
        And { lhs, rhs } | Or { lhs, rhs } | Xor { lhs, rhs } => {
            vec![("lhs", BOOL, lhs), ("rhs", BOOL, rhs)]
        }
        All(specs) | Any(specs) => specs.iter().map(|s| ("item", BOOL, s)).collect(),
        Not(inner) | BecameTrue(inner) | BecameFalse(inner) => vec![("source", BOOL, inner)],
        // Counts bars since a *signal* fired — `build` views it through
        // `into_bool`, exactly like `!if_else`'s `cond`.
        BarsSince { source } => vec![("source", BOOL, source)],
        // Polymorphic: a Bool inner (toggle) or a Real inner (any-change).
        Changed(inner) => vec![("source", BOOL_OR_REAL, inner)],
        // The string comparisons read a `Str` column on the left; `rhs` is a
        // `StrOperand`, not a `NodeSpec`.
        StrEq { lhs, .. } | StrNe { lhs, .. } => vec![("lhs", STR, lhs)],

        // The book-anchored recipes take a `source:` that is a *book selector*
        // (`!strategy_book` / `!portfolio_book`), not a value — excluded above
        // from `output_type` for the same reason.
        DrawdownThrottle { .. }
        | EquityVolTarget { .. }
        | FractionalKelly { .. }
        // Leaves and strategy-embedding tags with no typed expression child.
        | Pick { .. }
        | Value(_)
        | Entry
        | Peak
        | Trough
        | StrategyBook
        | PortfolioBook
        | Equity { .. }
        | EquityPeak { .. }
        | Drawdown { .. }
        | ReturnPerBar { .. }
        | TradePnl { .. }
        | TradeReturn { .. }
        | Sharpe { .. }
        | Sortino { .. }
        | Volatility { .. }
        | MaxDrawdown { .. }
        | Calmar { .. }
        // Absorbed leaf signals with no typed expression child.
        | Never
        | Every(_)
        | IsWeekday
        | IsWeekend
        | HasColumn { .. } => Vec::new(),
    }
}


/// The tag name of a variant, for diagnostics (`Sma` → `!sma`).
///
/// Read off the `Debug` derive's leading identifier rather than a 113-arm
/// table: the enum is externally tagged and its variant idents are the tag
/// names modulo case, so this stays correct for free as variants are added —
/// which a hand-written table would not.
pub(crate) fn tag_name(spec: &NodeSpec) -> String {
    snake_tag(&format!("{spec:?}"))
}

/// Every tag the [`NodeSpec`] layer accepts, in declaration order.
///
/// Same spirit as `tag_name`: read it off what serde already knows rather
/// than maintaining a parallel table. Feeding the deserializer a tag that
/// cannot exist makes its derived `unknown variant` error enumerate every
/// variant it *does* accept — so a new `NodeSpec` variant shows up here with no
/// edit, which is the whole point. A hand-written list is exactly the thing
/// that silently goes stale.
///
/// Names come back **without** the leading `!`. Since the value/signal split
/// was merged, this is the *one* node vocabulary — every tag (numeric source,
/// boolean predicate, string comparison) is a `NodeSpec` variant.
pub fn known_node_tags() -> Vec<String> {
    let v: serde_norway::Value = serde_norway::from_str(IMPOSSIBLE_TAG).expect("valid YAML");
    variants_from_error(&NodeSpec::try_from(v).expect_err("the tag cannot exist"))
}

/// [`known_node_tags`] for [`SelectionRuleSpec`](crate::spec::basket::SelectionRuleSpec)
/// — the third tag vocabulary, used by a `basket:` document's `selection:`.
pub fn known_selection_tags() -> Vec<String> {
    let e = serde_norway::from_str::<crate::spec::basket::SelectionRuleSpec>(IMPOSSIBLE_TAG)
        .expect_err("the tag cannot exist");
    variants_from_error(&e.to_string())
}

/// Tags accepted by the parser that are **not** enum variants, because a
/// load-time pass rewrites them before the typed parse ever runs.
///
/// `!equal_weight <N>` is sizing sugar lowered to `!value <1/N>` by
/// `rewrite_sugar_tags`; the rest are the `!import` / `!param` / `!arg` /
/// `!undefined` placeholders resolved by the load passes. They're legitimately
/// documented in `fugazi list` without appearing in any variant list — so
/// anything that cross-checks the catalogue against
/// `known_node_tags` has to know about them.
pub const REWRITTEN_TAGS: &[&str] = &["equal_weight", "param", "undefined", "import", "arg"];

/// A tag no variant will ever be named, used to provoke serde's
/// variant-listing error. The mapping body keeps the shape valid for tags that
/// take fields.
const IMPOSSIBLE_TAG: &str = "!__fugazi_no_such_tag__ {}";

/// Pull the backtick-quoted names out of serde's
/// ``unknown variant `x`, expected one of `a`, `b`, …`` message, dropping the
/// first (the unknown tag itself).
fn variants_from_error(message: &str) -> Vec<String> {
    let Some(list) = message.split_once("expected one of ").map(|(_, rest)| rest) else {
        panic!("serde's unknown-variant error changed shape: {message}");
    };
    let names: Vec<String> = list
        .split('`')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect();
    assert!(
        !names.is_empty(),
        "no variants parsed out of: {message}"
    );
    names
}

/// `"MacdLine { .. }"` → `"!macd_line"`.
///
/// Shared with [`grammar::default_expr_of`](crate::spec::grammar::default_expr_of),
/// which spells a defaulted slot's value the same way — off its `Debug`, so
/// neither has a table to go stale.
pub(super) fn snake_tag(debug: &str) -> String {
    let ident: String = debug
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect();
    // `MacdLine` → `macd_line`
    let mut out = String::with_capacity(ident.len() + 4);
    for (i, ch) in ident.chars().enumerate() {
        if ch.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    format!("!{out}")
}

/// Check only this node's immediate children.
///
/// The parse-time entry point. Children deserialize before their parent, so
/// each has already validated its own slots by the time this runs — meaning a
/// deep mismatch surfaces from the innermost node, where the message is most
/// useful, without this pass recursing.
pub fn check_immediate(spec: &NodeSpec) -> Result<(), String> {
    for (slot, expect, child) in children(spec) {
        if matches!(expect, Expect::OneOf([])) {
            continue;
        }
        let Some(actual) = output_type(child) else {
            continue;
        };
        if !expect.admits(actual) {
            return Err(format!(
                "{}'s `{slot}` expects a {} source, but {} produces {actual}",
                tag_name(spec),
                expect.describe(),
                tag_name(child),
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The tag-keyed view of the same table
// ---------------------------------------------------------------------------
//
// [`children`] answers "what does *this node* demand of its children", which is
// what `check_immediate` needs but not what a tooling consumer does: an editor
// completing `!and { lhs: ` has a tag and a slot name, not a tree. The demands
// are a property of the *variant* — every arm above returns a fixed list of
// constants — so the same table answers the tag-keyed question too, if you can
// get from a tag name to a node.
//
// Rather than hand-write a second table (which would drift, and lose the
// exhaustive-match guard that makes the first one trustworthy), synthesise one
// **prototype** node per tag from its own grammar record and run `children` on
// that. `spec::grammar` already knows every tag's shape, fields, and field
// types; filling each expression slot with a `!get` — whose `output_type` is
// `None`, so it satisfies any demand — makes a prototype that parses regardless
// of what the slot wants. One authority, no duplication.

/// A prototype value for a grammar field of type `ty`, as YAML source.
///
/// `None` for a field this cannot fabricate — today only `strategy` (an
/// embedded strategy document). Every tag carrying one is in `children`'s
/// "no typed expression child" arm, so there is nothing to observe on it
/// anyway, and `demand_table_covers_every_node_slot` pins that.
fn prototype_filler(ty: &str) -> Option<&'static str> {
    Some(match ty {
        // `!get`'s output is schema-dependent, hence undecidable, hence
        // accepted in every slot — the point of the filler is to parse, not to
        // typecheck. Spelled in the untagged single-key form (`{ get: … }`
        // rather than `!get …`) because YAML forbids two tags on one node, so
        // the tagged spelling would not parse in a `!not`/`!changed` payload.
        "node" => "{ get: { key: probe } }",
        "node_list" => "[{ get: { key: probe } }]",
        "match_cases" => "[{ when: 1, value: { get: { key: probe } } }]",
        "positive_uint" | "number" | "literal" => "1",
        "str" | "str_operand" => "probe",
        _ => return None,
    })
}

/// Build the minimal node that exercises every expression slot of `tag`.
///
/// Optional *node* fields are filled even though they could be omitted:
/// `children`'s `opt` helper reports nothing for an absent one, and an
/// unreported slot is exactly what this is trying to avoid.
fn prototype(tag: &GrammarTag) -> Option<NodeSpec> {
    // The canonical spelling only. An alternate form holds the same slots under
    // different syntax, so probing it would report the same demands twice.
    let form = tag.canonical();
    let body = match form.shape.as_str() {
        "unit" => String::new(),
        "newtype" | "seq" => format!(" {}", prototype_filler(form.payload.as_deref()?)?),
        "map" => {
            let mut parts = Vec::new();
            for field in &form.fields {
                let is_node = matches!(field.ty.as_str(), "node" | "node_list" | "match_cases");
                if !field.required && !is_node {
                    continue;
                }
                parts.push(format!("{}: {}", field.name, prototype_filler(&field.ty)?));
            }
            format!(" {{ {} }}", parts.join(", "))
        }
        _ => return None,
    };
    let text = format!("!{}{body}", tag.name);
    let value: serde_norway::Value = serde_norway::from_str(&text).ok()?;
    // `parse_unchecked`, not the `TryFrom` path: a prototype only has to have
    // the right *shape* for `children` to read its slots off, and running the
    // type check on a tree of `!get`s would just skip every one anyway.
    NodeSpec::parse_unchecked(value).ok()
}

/// One tag's expression slots, each with the output types it admits — the value
/// [`slot_demands`] returns and the demand table stores.
pub type SlotDemands = Vec<(&'static str, Vec<PayloadType>)>;

/// Every `(tag, slot) → demand` [`children`] encodes, keyed by tag name.
///
/// Built once. Reads `NodeSpec::grammar_tags()` — the derive's raw output —
/// rather than [`crate::spec::grammar::spec_grammar`], which calls back into
/// this to stamp its `node_output` fields.
fn demand_table() -> &'static BTreeMap<String, SlotDemands> {
    static TABLE: OnceLock<BTreeMap<String, SlotDemands>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = BTreeMap::new();
        for tag in NodeSpec::grammar_tags() {
            let Some(proto) = prototype(&tag) else {
                continue;
            };
            let mut slots: SlotDemands = Vec::new();
            for (slot, expect, _) in children(&proto) {
                // A variadic slot repeats — `!all`'s `item`, `!match`'s
                // `case value` — with the same demand each time. One entry.
                if !slots.iter().any(|(s, _)| *s == slot) {
                    slots.push((slot, expect.types()));
                }
            }
            table.insert(tag.name.clone(), slots);
        }
        table
    })
}

/// What `tag` requires the expression in `slot` to produce.
///
/// The tag-keyed face of the same type discipline `check` enforces — the
/// answer to "`!and`'s `lhs:` has to be *what*?" without a tree in hand.
/// `tag` may be written with or without its leading `!`; `slot` is the YAML
/// key (`source`, `lhs`, `high`, …), or the pseudo-slot a tag with no named
/// fields uses for its positional payload — `source` for `!not` / `!changed`,
/// `item` for `!all` / `!any`, `case value` for `!match`'s cases.
///
/// Three distinct answers:
///
/// * `None` — no such expression slot (an unknown tag, a scalar field like
///   `period:`, or a slot that holds no expression).
/// * `Some(&[])` — the slot holds an expression but demands nothing of its
///   output: `!unstable`'s `source` and `!resample`'s `inner` are
///   passthroughs, so any type is fine.
/// * `Some(types)` — one entry for an exact demand (`!and`'s `lhs` → `Bool`),
///   several for a slot admitting alternatives (`!changed`'s `source` →
///   `Bool` or `Real`, `!match`'s `on` → `Real` or `Str`).
///
/// ```
/// use fugazi::runtime::PayloadType;
/// use fugazi::spec::typecheck::slot_demand;
///
/// assert_eq!(slot_demand("and", "lhs"), Some(vec![PayloadType::Bool]));
/// assert_eq!(slot_demand("!sma", "source"), Some(vec![PayloadType::Real]));
/// assert_eq!(slot_demand("atr", "source"), Some(vec![PayloadType::Candle]));
/// assert_eq!(slot_demand("unstable", "source"), Some(vec![]));
/// assert_eq!(slot_demand("sma", "period"), None);
/// ```
pub fn slot_demand(tag: &str, slot: &str) -> Option<Vec<PayloadType>> {
    slot_demands(tag)
        .into_iter()
        .find(|(name, _)| *name == slot)
        .map(|(_, types)| types)
}

/// Every expression slot `tag` has, with each one's demand — the whole-tag
/// form of [`slot_demand`], in the order `children` reports them. Empty for a
/// tag with no expression slots (`!entry`, `!is_weekday`) and for an unknown
/// tag.
pub fn slot_demands(tag: &str) -> SlotDemands {
    demand_table()
        .get(tag.strip_prefix('!').unwrap_or(tag))
        .cloned()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::expr::Root;
    use crate::indicators::{Book, Position};
    use crate::types::Schema;

    /// Parse **without** the type check, so these tests can construct the
    /// ill-typed trees they exercise — the public parse path rejects them now.
    fn parse(yaml: &str) -> NodeSpec {
        let value: serde_norway::Value =
            serde_norway::from_str(yaml).unwrap_or_else(|e| panic!("parsing {yaml:?}: {e}"));
        NodeSpec::parse_unchecked(value).unwrap_or_else(|e| panic!("parsing {yaml:?}: {e}"))
    }

    /// Parse through the **public** path, which applies the check.
    fn parse_checked(yaml: &str) -> Result<NodeSpec, String> {
        let value: serde_norway::Value = serde_norway::from_str(yaml).expect("valid YAML");
        NodeSpec::try_from(value)
    }

    /// The drift guard: for a representative of every classification, the type
    /// this module *declares* must equal the type `build` actually produces.
    /// One representative per classification, shared by both drift tests.
    fn representatives() -> &'static [&'static str] {
        &[
            "!close {}",
            "!high {}",
            "!typical {}",
            "!current {}",
            "!pick { symbol: BTC }",
            "!value 3.0",
            "!value bull",
            "!time {}",
            "!year {}",
            "!unix_millis {}",
            "!ema { period: 3 }",
            "!sma { period: 3 }",
            "!stddev { period: 3 }",
            "!percentile { period: 3, pct: 0.5 }",
            "!percentile_rank { period: 3 }",
            "!bars_since_high { period: 3 }",
            "!bars_since { source: !value true }",
            "!atr { period: 3 }",
            "!obv",
            "!true_range",
            "!adx { period: 3 }",
            "!aroon_up { period: 3 }",
            "!macd_line { fast: 2, slow: 3, signal: 2 }",
            "!bb_upper { period: 3, k: 2.0 }",
            "!keltner_upper { ema_period: 3, atr_period: 3, multiplier: 2.0 }",
            "!donchian_upper { period: 3 }",
            "!sar { step: 0.02, max: 0.2 }",
            "!add { lhs: close, rhs: close }",
            "!lag { period: 1 }",
            "!log { base: 2.718281828459045 }",
            "!exp { base: 2.718281828459045 }",
            "!rolling_max { period: 3 }",
            "!latch { source: close }",
            "!unstable { source: close }",
            "!resample { every: 2, inner: !sma { period: 2 } }",
            "!if_else { cond: !value true, then: close, otherwise: close }",
            "!correlation { lhs: close, rhs: close, period: 3 }",
            "!vol_target { target: 0.2, window: 3, bars_per_year: 365.0 }",
            "!drawdown",
            "!equity",
            "!entry",
            // absorbed boolean signals
            "!gt { lhs: close, rhs: close }",
            "!above { source: close, level: 1.0 }",
            "!crosses_above { lhs: close, rhs: close }",
            "!and { lhs: !value true, rhs: !value true }",
            "!str_eq { lhs: !value bull, rhs: bear }",
            "!changed { source: close }",
            "!every 5",
            "!is_weekday",
        ]
    }

    #[test]
    fn declared_output_type_matches_what_build_produces() {
        let anchor = Position::new();
        let book = Book::new(1.0);
        let schema = Schema::empty();
        for yaml in representatives() {
            let spec = parse(yaml);
            let Some(declared) = output_type(&spec) else {
                panic!("{yaml}: expected a declared output type");
            };
            let built = spec.build(&anchor, &book, None, &schema, Root::sole()).output_type();
            assert_eq!(
                declared, built,
                "{yaml}: declared {declared} but build produced {built}"
            );
        }
    }

    /// The other half of the drift guard, and the important one.
    ///
    /// [`declared_output_type_matches_what_build_produces`] pins what each tag
    /// *produces*; this pins what each tag *demands*, which is the half that
    /// would otherwise be an unverified copy of the `build` arms. For every
    /// representative and every slot the table claims is typed, substituting a
    /// child of a forbidden type must make `try_build` reject it. If the engine
    /// ever stops demanding that type — a slot loosened from `real(x)` to
    /// something polymorphic, or a variant moved between families — the
    /// substitution starts succeeding and this test fails, rather than the
    /// checker silently reporting a constraint the engine no longer enforces.
    ///
    /// This used to need `catch_unwind`, because the engine's enforcement *was*
    /// an `assert_eq!`. Now that `try_build` returns the mismatch as a value,
    /// the test just reads the `Err`.
    #[test]
    fn declared_child_expectations_match_what_build_demands() {
        let anchor = Position::new();
        let book = Book::new(1.0);
        let schema = Schema::empty();

        let mut checked = 0usize;
        let mut stale: Vec<String> = Vec::new();
        for yaml in representatives() {
            let spec = parse(yaml);
            for (slot, expect, _) in children(&spec) {
                // `Match`'s per-case slot isn't addressable as a single key,
                // and the passthrough marker demands nothing.
                if slot.contains(' ') || matches!(expect, Expect::OneOf([])) {
                    continue;
                }
                // A tag whose output the slot forbids.
                let wrong = if expect.admits(PayloadType::Real) {
                    "!current {}" // Candle
                } else {
                    "!value 1.0" // Real
                };
                let Some(mutated) = with_slot(yaml, slot, wrong) else {
                    continue;
                };
                if mutated
                    .try_build(&anchor, &book, None, &schema, Root::sole())
                    .is_ok()
                {
                    stale.push(format!(
                        "{yaml}: table says `{slot}` must be {}, but `build` accepted \
                         {wrong} there",
                        expect.describe(),
                    ));
                }
                checked += 1;
            }
        }
        assert!(
            stale.is_empty(),
            "declared child expectations no longer match the engine:\n  {}",
            stale.join("\n  "),
        );
        // Guards against the loop quietly becoming a no-op (a representative
        // list that stops parsing, a `with_slot` that stops matching): the
        // test would otherwise "pass" while checking nothing.
        assert!(
            checked >= 25,
            "expected to pin most typed slots, pinned only {checked}"
        );
    }

    /// Replace `slot` in a tagged-mapping spec with `replacement`, returning the
    /// re-parsed spec. `None` when the representative has no mapping body to
    /// edit (a bare `!obv`), which simply isn't a case this test can mutate.
    fn with_slot(yaml: &str, slot: &str, replacement: &str) -> Option<NodeSpec> {
        use serde_norway::Value as Y;
        let Y::Tagged(mut tagged) = serde_norway::from_str::<Y>(yaml).ok()? else {
            return None;
        };
        let repl: Y = serde_norway::from_str(replacement).ok()?;
        match &mut tagged.value {
            Y::Mapping(map) => {
                map.insert(Y::String(slot.to_string()), repl);
            }
            // `!obv` with no body — give it one.
            Y::Null => {
                let mut map = serde_norway::Mapping::new();
                map.insert(Y::String(slot.to_string()), repl);
                tagged.value = Y::Mapping(map);
            }
            _ => return None,
        }
        NodeSpec::parse_unchecked(Y::Tagged(tagged)).ok()
    }

    /// The undecidable cases must report `None` — *skip*, not a wrong guess.
    #[test]
    fn schema_dependent_and_selector_nodes_are_unknown() {
        assert_eq!(output_type(&parse("!get { key: regime }")), None);
        assert_eq!(output_type(&parse("!strategy_book")), None);
        assert_eq!(output_type(&parse("!portfolio_book")), None);
        // A passthrough over an unknown stays unknown.
        assert_eq!(
            output_type(&parse("!unstable { source: !get { key: x } }")),
            None
        );
    }

    #[test]
    fn accepts_well_typed_trees() {
        for yaml in [
            "!sma { period: 3, source: close }",
            "!atr { period: 3, source: !current {} }",
            "!close { source: !pick { symbol: BTC } }",
            "!add { lhs: !sma { period: 2 }, rhs: !ema { period: 3 } }",
            "!keltner_upper { ema_period: 3, atr_period: 3, multiplier: 2.0, source: close, candle_source: !current {} }",
        ] {
            assert!(parse_checked(yaml).is_ok(), "{yaml} should type-check");
        }
    }

    #[test]
    fn rejects_a_str_source_where_real_is_required() {
        let err = parse_checked("!sma { period: 3, source: !value bull }")
            .expect_err("Str into a Real slot");
        assert!(err.contains("!sma's `source`"), "{err}");
        assert!(err.contains("expects a Real source"), "{err}");
        assert!(err.contains("produces Str"), "{err}");
    }

    #[test]
    fn rejects_a_candle_source_where_real_is_required() {
        let err = parse_checked("!add { lhs: !current {}, rhs: !value 1.0 }")
            .expect_err("Candle into a Real slot");
        assert!(err.contains("!add's `lhs`"), "{err}");
        assert!(err.contains("produces Candle"), "{err}");
    }

    #[test]
    fn rejects_a_real_source_where_a_candle_is_required() {
        let err = parse_checked("!atr { period: 3, source: !sma { period: 2 } }")
            .expect_err("Real into a Candle slot");
        assert!(err.contains("expects a Candle source"), "{err}");
    }

    #[test]
    fn rejects_a_candle_source_where_an_atom_is_required() {
        let err =
            parse_checked("!close { source: !current {} }").expect_err("Candle into an Atom slot");
        assert!(err.contains("expects a Atom source"), "{err}");
    }

    #[test]
    fn reports_the_innermost_mismatch() {
        // The outer `!sma` would also be unhappy (its source can't be built),
        // but children parse first, so the reported error is the inner `!add`'s
        // — the one that actually names the mistake.
        let err =
            parse_checked("!sma { period: 3, source: !add { lhs: !value bull, rhs: close } }")
                .expect_err("nested mismatch");
        assert!(err.contains("!add's `lhs`"), "{err}");
        assert!(err.contains("produces Str"), "{err}");
    }

    #[test]
    fn rejects_a_non_real_source_in_a_signal_slot() {
        // The signal layer is where a source is first *used*, so this is the
        // likeliest place a mismatch lands — and it went unchecked until an
        // `!above { source: !pick { … } }` reached `AsReal`'s assert mid-run.
        let sig: Result<NodeSpec, String> = serde_norway::from_str::<serde_norway::Value>(
            "!above { source: !pick { symbol: BTC }, level: 0.0 }",
        )
        .map_err(|e| e.to_string())
        .and_then(NodeSpec::try_from);
        let err = sig.expect_err("Atom into a Real slot");
        assert!(err.contains("!above's `source`"), "{err}");
        assert!(err.contains("produces Atom"), "{err}");
    }

    #[test]
    fn signal_comparisons_reject_a_candle_operand() {
        for yaml in [
            "!gt { lhs: !current {}, rhs: close }",
            "!crosses_above { lhs: close, rhs: !current {} }",
        ] {
            let sig: Result<NodeSpec, String> =
                serde_norway::from_str::<serde_norway::Value>(yaml)
                    .map_err(|e| e.to_string())
                    .and_then(NodeSpec::try_from);
            assert!(sig.is_err(), "{yaml} should be rejected");
        }
    }

    #[test]
    fn eq_admits_both_real_and_str_operands() {
        // `!eq` dispatches on the left operand's type, so neither Real nor Str
        // may be rejected here — only something no comparison accepts.
        for yaml in [
            "!eq { lhs: close, rhs: !value 5.0 }",
            "!eq { lhs: !value bull, rhs: !value bear }",
        ] {
            let sig: Result<NodeSpec, String> =
                serde_norway::from_str::<serde_norway::Value>(yaml)
                    .map_err(|e| e.to_string())
                    .and_then(NodeSpec::try_from);
            assert!(sig.is_ok(), "{yaml} should type-check: {sig:?}");
        }
    }

    #[test]
    fn an_unknown_child_is_skipped_not_rejected() {
        // `!get`'s type needs the schema, so this must pass rather than guess.
        assert!(parse_checked("!sma { period: 3, source: !get { key: x } }").is_ok());
    }

    // --- the tag-keyed view ------------------------------------------------

    /// The `source:` slots that take a **book selector** (`!strategy_book` /
    /// `!portfolio_book`), not a value expression. `children` excludes them on
    /// purpose, so `slot_demand` reports `None` — "not an expression slot" —
    /// rather than `Some(&[])`, which would read as "any type is fine".
    const BOOK_SELECTOR_SLOTS: &[&str] = &[
        "equity",
        "equity_peak",
        "drawdown",
        "return_per_bar",
        "trade_pnl",
        "trade_return",
        "drawdown_throttle",
        "equity_vol_target",
        "fractional_kelly",
    ];

    #[test]
    fn slot_demand_answers_the_tag_keyed_question() {
        use PayloadType::*;
        // An exact demand, with and without the leading `!`.
        assert_eq!(slot_demand("and", "lhs"), Some(vec![Bool]));
        assert_eq!(slot_demand("!and", "rhs"), Some(vec![Bool]));
        assert_eq!(slot_demand("sma", "source"), Some(vec![Real]));
        assert_eq!(slot_demand("atr", "source"), Some(vec![Candle]));
        assert_eq!(slot_demand("close", "source"), Some(vec![Atom]));
        assert_eq!(slot_demand("str_eq", "lhs"), Some(vec![Str]));
        // Alternatives.
        assert_eq!(slot_demand("changed", "source"), Some(vec![Bool, Real]));
        assert_eq!(slot_demand("match", "on"), Some(vec![Real, Str]));
        // Passthrough — an expression slot that demands nothing.
        assert_eq!(slot_demand("unstable", "source"), Some(vec![]));
        assert_eq!(slot_demand("resample", "inner"), Some(vec![]));
        // Not an expression slot at all.
        assert_eq!(slot_demand("sma", "period"), None);
        assert_eq!(slot_demand("drawdown", "source"), None);
        assert_eq!(slot_demand("no_such_tag", "source"), None);
        // The positional-payload pseudo-slots.
        assert_eq!(slot_demand("not", "source"), Some(vec![Bool]));
        assert_eq!(slot_demand("all", "item"), Some(vec![Bool]));
        assert_eq!(slot_demand("match", "case value"), Some(vec![Real]));
    }

    /// The prototype pass is only as good as its coverage: a tag whose
    /// prototype fails to build reports *no* demands, which looks exactly like
    /// a tag that has none. Pin it — every expression-holding field of every
    /// node tag must either carry a demand or be a known book selector.
    #[test]
    fn demand_table_covers_every_node_slot() {
        let mut unreported = Vec::new();
        for tag in NodeSpec::grammar_tags() {
            let slots = slot_demands(&tag.name);
            // Every form: an alternate spelling exposes the same slots under
            // different syntax, and a consumer completing inside one still needs
            // the demand, so an unreported slot there is the same hole.
            for form in &tag.forms {
                for field in &form.fields {
                    let slot = match field.ty.as_str() {
                        "node" | "node_list" => field.name.as_str(),
                        "match_cases" => "case value",
                        _ => continue,
                    };
                    if slots.iter().any(|(s, _)| *s == slot) {
                        continue;
                    }
                    if BOOK_SELECTOR_SLOTS.contains(&tag.name.as_str()) && slot == "source" {
                        continue;
                    }
                    unreported.push(format!("!{} `{}`", tag.name, field.name));
                }
                if matches!(form.payload.as_deref(), Some("node" | "node_list")) && slots.is_empty()
                {
                    unreported.push(format!("!{} payload", tag.name));
                }
            }
        }
        assert!(
            unreported.is_empty(),
            "expression slots with no reported demand — either `children` is \
             missing an arm, or `prototype` cannot build these tags:\n  {}",
            unreported.join("\n  "),
        );
    }

    /// Every demand the tag-keyed table reports has to be the one `children`
    /// reports for a real tree — the prototypes are a shortcut to the same
    /// table, not a second copy of it.
    #[test]
    fn the_tag_keyed_table_agrees_with_children() {
        for (spec, slot, want) in [
            ("!and { lhs: !is_weekday, rhs: !is_weekend }", "lhs", "Bool"),
            ("!ema { source: !close {}, period: 3 }", "source", "Real"),
            ("!adx { source: !current {}, period: 3 }", "source", "Candle"),
        ] {
            let node = parse(spec);
            let from_tree = children(&node)
                .into_iter()
                .find(|(s, _, _)| *s == slot)
                .map(|(_, e, _)| e.describe())
                .expect("slot present");
            assert_eq!(from_tree, want);
            let from_tag = slot_demand(&tag_name(&node), slot).expect("slot present");
            assert_eq!(from_tag.iter().map(|t| t.to_string()).collect::<Vec<_>>(), vec![want]);
        }
    }

    /// Both were absent from `children` while `build` demanded a Bool through
    /// `into_bool` — so a real mistake surfaced only mid-build.
    #[test]
    fn bool_slots_that_used_to_slip_through_are_checked() {
        let err = parse_checked("!bars_since { source: !sma { period: 3 } }").unwrap_err();
        assert!(err.contains("expects a Bool source"), "{err}");
        let err = parse_checked(
            "!if_else { cond: !sma { period: 3 }, then: !close {}, otherwise: !close {} }",
        )
        .unwrap_err();
        assert!(err.contains("expects a Bool source"), "{err}");
        // The valid spellings still parse.
        assert!(parse_checked("!bars_since { source: !is_weekday }").is_ok());
    }
}

