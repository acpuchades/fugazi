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
//! [`output_type`] returns `Option<DynType>`, and `None` means **unknown —
//! skip**, never "invalid". Three things are genuinely undecidable without
//! data or a driver:
//!
//! * `!get { key }` resolves its type against the overlay [`Schema`], which
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

use crate::runtime::DynType;
use crate::spec::expr::{NodeSpec, ValueLit};

/// What a child slot is allowed to produce.
#[derive(Debug, Clone, Copy)]
enum Expect {
    /// Exactly this type.
    Only(DynType),
    /// Any of these — `!match`'s `on:` dispatches on a number *or* a string.
    OneOf(&'static [DynType]),
}

impl Expect {
    fn admits(self, ty: DynType) -> bool {
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
}

const REAL: Expect = Expect::Only(DynType::Real);
const CANDLE: Expect = Expect::Only(DynType::Candle);
const ATOM: Expect = Expect::Only(DynType::Atom);
const BOOL: Expect = Expect::Only(DynType::Bool);
/// `!changed` accepts either a Bool inner (toggle) or a Real inner (any-change).
const BOOL_OR_REAL: Expect = Expect::OneOf(&[DynType::Bool, DynType::Real]);
/// A string comparison's `lhs` reads a `Str` column.
const STR: Expect = Expect::Only(DynType::Str);
/// `!match`'s `on:` — numeric or string dispatch (the two are not mixable
/// within one `!match`, but that is a build-time check against the cases).
const REAL_OR_STR: Expect = Expect::OneOf(&[DynType::Real, DynType::Str]);

/// What this expression produces, or `None` when that cannot be known
/// statically (see the module docs — `None` means *skip*, never *invalid*).
pub fn output_type(spec: &NodeSpec) -> Option<DynType> {
    use NodeSpec::*;
    match spec {
        // --- the non-Real leaves ---
        Current { .. } => Some(DynType::Candle),
        Pick { .. } => Some(DynType::Atom),
        Time { .. } => Some(DynType::Time),
        Value(ValueLit::Str(_)) => Some(DynType::Str),
        Value(ValueLit::Real(_)) => Some(DynType::Real),
        Value(ValueLit::Bool(_)) => Some(DynType::Bool),
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
        | HasColumn { .. } => Some(DynType::Bool),

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
        | UnixMillis { .. } => Some(DynType::Real),
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
        Correlation { lhs, rhs, .. } | Add { lhs, rhs } | Sub { lhs, rhs } | Mul { lhs, rhs }
        | Div { lhs, rhs } => vec![("lhs", REAL, lhs), ("rhs", REAL, rhs)],

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
            then, otherwise, ..
        } => vec![("then", REAL, then), ("otherwise", REAL, otherwise)],

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
        | BarsSince { .. }
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
/// Same spirit as [`tag_name`]: read it off what serde already knows rather
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
/// [`known_expr_tags`] has to know about them.
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
fn snake_tag(debug: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
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
            let built = spec.build(&anchor, &book, None, &schema, None).output_type();
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
                let wrong = if expect.admits(DynType::Real) {
                    "!current {}" // Candle
                } else {
                    "!value 1.0" // Real
                };
                let Some(mutated) = with_slot(yaml, slot, wrong) else {
                    continue;
                };
                if mutated
                    .try_build(&anchor, &book, None, &schema, None)
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
}
