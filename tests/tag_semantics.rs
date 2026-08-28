//! **Every expression tag builds, reads, and reads *differently* from every
//! other one.**
//!
//! `tests/spec_grammar.rs` proves each declared form *parses*. Nothing proved
//! what the parsed tag then *builds*: `NodeSpec::try_build` is one long match,
//! and an arm pointing at the wrong constructor or the wrong component accessor
//! is invisible to a parse guard. Most of the vocabulary had no other cover
//! either — 97 of the 157 node tags appear in no behavioural test as YAML at
//! all, reaching the suite only through the reflection guards. Wiring
//! `!bb_upper` to the *lower* band and `!aroon_up` to `aroon_down` both left
//! `cargo test` green.
//!
//! Three properties, in increasing strength:
//!
//! 1. **It builds and reads.** A tag that stopped producing a value would
//!    otherwise surface as a silently blank overlay column.
//! 2. **No two tags are the same function.** This is what catches the dominant
//!    bug shape — a component accessor pointing at its sibling's field, or a
//!    constructor swapped for its neighbour's. Two tags with different names
//!    that read identically over an adversarial stream are either a wiring
//!    mistake or an undeclared alias; [`ALIASES`] is where a real alias says so.
//! 3. **The banded families are ordered.** `lower <= middle <= upper` on every
//!    bar, which catches an upper/lower swap a second way and does not depend
//!    on the sibling being present.
//!
//! What this file deliberately does *not* do is re-derive each indicator's
//! arithmetic — that is `tests/indicator_reference.rs` and
//! `tests/talib_validation.rs`. The subject here is the mapping from tag to
//! indicator.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use fugazi::market::{OverlayInfo, OverlayValue, Real, Schema};
use fugazi::runtime::{PayloadIndicator, PayloadValue};
use fugazi::spec::grammar::{GrammarForm, GrammarTag, spec_grammar};
use fugazi::spec::overlay::build_overlay;
use fugazi::spec::{NodeSpec, Root};
use fugazi::types::{Atom, Candle, Snapshot, Symbol, symbol as intern};

const SYMBOL: &str = "X";
/// The one overlay column the `!get` / `!has_column` probes read.
const PROBE_COLUMN: &str = "probe";

/// Tags that cannot be probed from the grammar alone, each with the reason and
/// what covers them instead.
///
/// Kept deliberately short: an entry here is a hole in the sweep, so it has to
/// earn its place.
const UNPROBEABLE: &[(&str, &str)] = &[
    (
        "param",
        "a load-time placeholder, rewritten before the typed parse ever runs — \
         covered by src/spec/params.rs and tests/param_types.rs",
    ),
    (
        "slot",
        "as `param`, but the build-time pass — covered by src/spec/slots.rs and \
         src/spec/template.rs",
    ),
    (
        "import",
        "a load-time filesystem splice — covered by src/spec/imports.rs",
    ),
    (
        "undefined",
        "the typed hole `check` substitutes for an unresolved placeholder; it \
         builds to a zero and is not a runnable value — covered by \
         src/spec/undefined.rs and tests/check_builds.rs",
    ),
    (
        "equal_weight",
        "sizing sugar lowered to `!value <1/N>` at load — covered by \
         `rewrite_sugar_tags` in src/spec/expr.rs",
    ),
    (
        "strategy_book",
        "a build-time *source selector*, not an expression: it only means \
         anything as the `source:` of a book-reading tag, and every one of \
         those (`!equity`, `!drawdown`, …) is probed here with its default — \
         covered by `resolve_book_source` in src/spec/expr.rs and by \
         tests/portfolio.rs",
    ),
    ("portfolio_book", "as `strategy_book`"),
];

/// Categories whose tags read **live run state** — an open position, a traded
/// book — which `build_overlay` deliberately stubs out (`Position::new()`,
/// `Book::new(1.0)`). Outside a run they are constant or silent, so neither the
/// "reads" nor the "distinct" sweep can say anything about them.
///
/// They are covered where the state exists: `src/indicators/position.rs` and
/// `src/indicators/book.rs` unit-test the leaves, `tests/portfolio.rs` and
/// `tests/ruin.rs` drive them through real runs, and `resolve_book_source`'s
/// `!strategy_book` / `!portfolio_book` selection is pinned in
/// `src/spec/portfolio.rs`.
const STUB_ANCHORED_CATEGORIES: &[&str] = &["position anchors", "strategy book"];

/// Individual tags in an otherwise-live category that read the same stubbed
/// state. The three book-anchored sizing recipes each collapse to a constant
/// against `Book::new(1.0)`; their siblings `!vol_target` and `!atr_risk` read
/// prices and stay in the sweep. Covered by `src/indicators/sizing.rs`'s unit
/// tests and by `tests/sizing_bootstrap.rs`, which drive a real book.
const STUB_ANCHORED_TAGS: &[&str] = &["drawdown_throttle", "equity_vol_target", "fractional_kelly"];

/// Pairs of tags that genuinely read the same over this fixture, with the
/// reason each is not a wiring mistake.
///
/// The distinctness sweep is only as good as this list is short. An entry means
/// "these two really are the same function here", **not** "this pair kept
/// failing".
const ALIASES: &[(&str, &str, &str)] = &[
    (
        "all",
        "and",
        "`!all [a, b]` is the n-ary spelling of `!and { lhs: a, rhs: b }` — the \
         same function by construction over a two-element list, which is what \
         the probe builds",
    ),
    ("any", "or", "as `!all` / `!and`"),
];

/// An adversarial-but-plain candle stream: a rise, a gap down, a flat stretch,
/// a spike and a fall, with volume that varies independently of price.
///
/// Varied enough that two genuinely different indicators separate on it, and
/// long enough that a 5-bar window plus a recursive smoother's settling tail
/// both get past their warm-up.
fn candles() -> Vec<Candle> {
    let closes: Vec<Real> = (0..60)
        .map(|i| {
            let i = i as Real;
            match i as usize {
                0..=19 => 100.0 + i * 2.0,
                20..=29 => 120.0,
                30..=39 => 138.0 - (i - 30.0) * 4.0,
                _ => 98.0 + (i - 40.0) * 1.5 + 6.0 * (i * 0.7).sin(),
            }
        })
        .collect();
    let mut prev = closes[0];
    closes
        .iter()
        .enumerate()
        .map(|(i, &close)| {
            let open = prev;
            prev = close;
            let pad = 1.0 + (i % 5) as Real;
            Candle::new(
                open,
                open.max(close) + pad,
                open.min(close) - pad,
                close,
                500.0 + 90.0 * ((i % 7) as Real),
            )
        })
        .collect()
}

/// The schema every probe builds against — one real column, so `!get` and
/// `!has_column` have something to name.
fn schema() -> Arc<Schema> {
    let mut b = Schema::builder();
    b.add_real(PROBE_COLUMN);
    b.finish()
}

/// The stream, as tagged snapshots carrying the probe column.
fn stream(schema: &Arc<Schema>) -> Vec<Snapshot<Symbol>> {
    let sym = intern(SYMBOL);
    // Five days, one hour, one minute and seven seconds apart. The odd stride is
    // what keeps the calendar tags apart: a whole-day cadence pins `hour`,
    // `minute` and `second` at zero — three tags reading the same constant —
    // and a sub-day one never advances `month` or `quarter`. The seconds are
    // seven rather than one so `!minute` and `!second` do not advance in
    // lockstep and read the same series. This moves every field of the
    // decomposition over the 60 bars.
    let stride = 5 * 86_400_000i64 + 3_600_000 + 60_000 + 7_000;
    candles()
        .into_iter()
        .enumerate()
        .map(|(i, candle)| {
            let overlays = OverlayInfo::new(
                Arc::clone(schema),
                [OverlayValue::Real(candle.close / 10.0)],
            );
            let atom = Atom::with_overlays_and_time(
                candle,
                overlays,
                fugazi::Timestamp(1_704_067_200_000 + i as i64 * stride),
            );
            Snapshot::single(sym.clone(), atom)
        })
        .collect()
}

/// Two boolean expressions for the slots that demand one: "this bar rose" and
/// "the close is above its 5-bar mean".
///
/// They have to **overlap partially** rather than be each other's negation. Two
/// mutually exclusive conditions make `!or` and `!xor` the same function, so
/// the pair would say nothing about which of the two a tag is wired to.
fn bool_node(second: bool) -> serde_json::Value {
    use serde_json::json;
    if second {
        json!({ "gt": { "lhs": { "close": null }, "rhs": { "sma": { "period": 5 } } } })
    } else {
        json!({ "gt": { "lhs": { "close": null }, "rhs": { "open": null } } })
    }
}

/// A stand-in JSON value for one grammar field, by type and by name.
///
/// Names matter: `fast` / `slow` / `signal` have to differ or the three MACD
/// projections collapse onto each other, and `pct` has to lie in `[0, 1]`.
///
/// `demands` is the field's `node_output` — what the slot requires a nested
/// expression to *produce*. A `["bool"]` slot handed a `!close` is a parse
/// error, so the filler reads it rather than guessing.
fn field_value(
    tag: &str,
    ty: &str,
    name: &str,
    demands: Option<&[String]>,
) -> Option<serde_json::Value> {
    use serde_json::json;
    let wants_bool = demands.is_some_and(|d| !d.is_empty() && d.iter().all(|o| o == "bool"));
    Some(match (ty, name) {
        ("node", "rhs") if wants_bool => bool_node(true),
        ("node", _) if wants_bool => bool_node(false),
        // The pointwise transforms saturate on a price: `sign`, `tanh` and
        // `sigmoid` all read 1.0 at a close of 120, so three different
        // functions would look like one. They get a small signed source
        // instead — the close's departure from its own 5-bar mean, scaled.
        ("node", "source") if matches!(tag, "abs" | "sign" | "tanh" | "sigmoid") => json!({
            "div": {
                "lhs": { "sub": { "lhs": { "close": null },
                                  "rhs": { "sma": { "period": 5 } } } },
                "rhs": { "value": 5.0 },
            }
        }),
        // …and `sqrt` / `log` / `exp` need a *positive* one small enough that
        // `exp` does not overflow to the infinity `pow` also reaches.
        ("node", "source") if matches!(tag, "sqrt" | "log" | "exp") => json!({
            "div": { "lhs": { "close": null }, "rhs": { "value": 50.0 } }
        }),
        // `close ^ open` is +inf, which is where `!exp` would land too.
        ("node", "rhs") if tag == "pow" => json!({ "value": 2.0 }),
        ("node_list", _) if wants_bool => json!([bool_node(false), bool_node(true)]),
        // Positional, so a two-operand tag is not handed the same expression
        // twice — `!max { lhs: close, rhs: close }` is `!close`, and half the
        // vocabulary would collapse onto it.
        ("node", "rhs" | "otherwise") => json!({ "open": null }),
        ("node", "high" | "upper") => json!({ "high": null }),
        ("node", "low" | "lower") => json!({ "low": null }),
        ("node", "then" | "default") => json!({ "typical": null }),
        ("node", _) => json!({ "close": null }),
        ("node_list", _) => json!([{ "close": null }, { "open": null }]),
        ("match_cases", _) => json!([{ "when": 1, "value": { "median": null } }]),
        (_, "fast") => json!(3),
        (_, "slow") => json!(7),
        (_, "signal") => json!(4),
        ("positive_uint" | "uint", _) => json!(5),
        (_, "pct") => json!(0.75),
        (_, "step") => json!(0.02),
        (_, "max") => json!(0.2),
        (_, "base") => json!(10.0),
        // A bucket has to span several base bars or the resampling tags
        // degenerate into the identity: one bar per bucket makes
        // `!dollar_bars { inner: close }` just `!close`. Volume runs ~500-1040
        // a bar and notional ~60 000, hence the two scales.
        ("number", "threshold") if tag == "volume_bars" => json!(2_000.0),
        ("number", "threshold") => json!(200_000.0),
        (_, "level") => json!(110.0),
        (_, "bars_per_year") => json!(365.0),
        (_, "target" | "max_drawdown") => json!(0.2),
        (_, "risk_frac") => json!(0.01),
        (_, "kelly_fraction") => json!(0.5),
        ("number" | "literal", _) => json!(2.0),
        ("str" | "str_operand", _) => json!(PROBE_COLUMN),
        ("str_list", _) => json!([SYMBOL]),
        ("number_list", _) => json!([1.0, 2.0]),
        ("bool", _) => json!(true),
        ("symbol", _) => json!(SYMBOL),
        ("frequency", _) => json!("1d"),
        // A preset that trades on the first bar, so the trailing risk tags have
        // a moving equity curve to measure rather than a flat one.
        ("strategy", _) => json!({ "buy_and_hold": { "root": SYMBOL } }),
        _ => return None,
    })
}

/// A minimal document for one form, in the JSON bridge encoding.
///
/// Required fields only: an omitted optional slot exercises the tag's *own*
/// default, which is the shape a user writes.
fn document(name: &str, form: &GrammarForm) -> Option<serde_json::Value> {
    use serde_json::json;
    let body = match form.shape.as_str() {
        "unit" => serde_json::Value::Null,
        "newtype" | "seq" => field_value(
            name,
            form.payload.as_deref()?,
            "",
            form.payload_output.as_deref(),
        )?,
        "map" => {
            let mut body = serde_json::Map::new();
            for f in &form.fields {
                if !f.required {
                    continue;
                }
                body.insert(
                    f.name.clone(),
                    field_value(name, &f.ty, &f.name, f.node_output.as_deref())?,
                );
            }
            serde_json::Value::Object(body)
        }
        other => panic!("!{name}: unknown form shape {other}"),
    };
    Some(json!({ name: body }))
}

/// Build `tag`'s minimal document and read it once per bar.
///
/// `None` when the tag cannot be probed at all; `Some(Err(_))` when it parsed
/// but would not build.
fn readings(tag: &GrammarTag) -> Option<Result<Vec<Option<String>>, String>> {
    let form = tag.forms.first()?;
    let doc = document(&tag.name, form)?;
    let spec: NodeSpec = match serde_json::from_value(doc.clone()) {
        Ok(s) => s,
        Err(e) => return Some(Err(format!("parse: {e} ({doc})"))),
    };
    let schema = schema();
    let mut ind = match build_overlay(&spec, &schema, Root::sole()) {
        Ok(i) => i,
        Err(e) => return Some(Err(format!("build: {e} ({doc})"))),
    };
    Some(Ok(drive(&mut *ind, &schema)))
}

/// One canonical string per bar — `None` for a bar the tag declined to answer.
///
/// Rendering to a string rather than comparing `PayloadValue`s keeps a `Real`
/// tag and a `Bool` tag trivially distinct, and gives a readable failure.
fn drive(ind: &mut dyn PayloadIndicator, schema: &Arc<Schema>) -> Vec<Option<String>> {
    stream(schema)
        .into_iter()
        .map(|snap| {
            ind.update(PayloadValue::Snapshot(snap)).map(|v| match v {
                PayloadValue::Real(r) => format!("r{r:.10}"),
                PayloadValue::Bool(b) => format!("b{b}"),
                PayloadValue::Str(s) => format!("s{s}"),
                PayloadValue::Time(t) => format!("t{}", t.0),
                PayloadValue::Candle(c) => format!("c{c:?}"),
                PayloadValue::Atom(a) => format!("a{:?}", a.candle),
                PayloadValue::Snapshot(_) => "snapshot".to_string(),
            })
        })
        .collect()
}

/// Every node tag the sweep can probe, with its category and per-bar readings.
fn probed_with_category() -> BTreeMap<String, (String, Vec<Option<String>>)> {
    let unprobeable: BTreeSet<&str> = UNPROBEABLE.iter().map(|(n, _)| *n).collect();
    let mut out = BTreeMap::new();
    let mut failed = Vec::new();
    for tag in spec_grammar() {
        if tag.group != "node" || unprobeable.contains(tag.name.as_str()) {
            continue;
        }
        match readings(&tag) {
            None => failed.push(format!("!{}: no probe document could be built", tag.name)),
            Some(Err(e)) => failed.push(format!("!{}: {e}", tag.name)),
            Some(Ok(values)) => {
                out.insert(tag.name.clone(), (tag.category.clone(), values));
            }
        }
    }
    assert!(
        failed.is_empty(),
        "these tags could not be probed — add a `field_value` stand-in, or an \
         `UNPROBEABLE` entry saying what covers them instead:\n  {}",
        failed.join("\n  "),
    );
    out
}

/// The readings alone, dropping the category.
fn probed() -> BTreeMap<String, Vec<Option<String>>> {
    probed_with_category()
        .into_iter()
        .map(|(k, (_, v))| (k, v))
        .collect()
}

/// A stale `UNPROBEABLE` entry reads as "covered elsewhere" for a tag that no
/// longer exists, which is worse than no entry at all.
#[test]
fn no_unprobeable_entry_names_a_tag_that_no_longer_exists() {
    let known: BTreeSet<String> = spec_grammar().into_iter().map(|t| t.name).collect();
    let stale: Vec<&str> = UNPROBEABLE
        .iter()
        .map(|(n, _)| *n)
        .filter(|n| !known.contains(*n))
        .collect();
    assert!(
        stale.is_empty(),
        "UNPROBEABLE names unknown tags: {stale:?}"
    );
}

/// **Every tag builds and produces at least one reading.**
///
/// A tag that stopped answering would show up as a permanently blank overlay
/// column — which `fugazi get` warns about at runtime precisely because it is
/// indistinguishable from a long warm-up.
#[test]
fn every_expression_tag_builds_and_reads() {
    let probed = probed();
    assert!(
        probed.len() > 130,
        "only {} tags were probed — the sweep has narrowed",
        probed.len()
    );

    let with_category = probed_with_category();
    let silent: Vec<&str> = with_category
        .iter()
        .filter(|(_, (cat, _))| !STUB_ANCHORED_CATEGORIES.contains(&cat.as_str()))
        .filter(|(_, (_, v))| v.iter().all(Option::is_none))
        .map(|(k, _)| k.as_str())
        .collect();
    assert!(
        silent.is_empty(),
        "these tags built but never produced a value over the probe stream, \
         which is indistinguishable from a blank column: {silent:?}",
    );
}

/// **No two tags in the same category are the same function.**
///
/// Scoped to the grammar's own `category` — the taxonomy `spec_grammar` already
/// pins to cover every tag exactly once — because that is where the bug lives
/// and where a collision means something. `!bb_upper` wired to the lower band
/// collides with `!bb_lower`; `!plus_di` wired to `minus_di` collides with
/// `!minus_di`; `!aroon_up` with `!aroon_down`; `!linreg_slope` with
/// `!linreg_intercept`. Comparing *across* categories would instead surface a
/// long tail of accidental agreements (`!latch` of a source that never declines
/// to answer is the identity, and so is `!unstable`) that say nothing about
/// wiring.
#[test]
fn no_two_tags_in_a_category_read_identically() {
    let probed = probed_with_category();
    let allowed: BTreeSet<(&str, &str)> = ALIASES.iter().map(|(a, b, _)| (*a, *b)).collect();

    let mut by_category: BTreeMap<&str, Vec<&String>> = BTreeMap::new();
    for (name, (category, _)) in &probed {
        if STUB_ANCHORED_CATEGORIES.contains(&category.as_str())
            || STUB_ANCHORED_TAGS.contains(&name.as_str())
        {
            continue;
        }
        by_category.entry(category.as_str()).or_default().push(name);
    }

    let mut collisions = Vec::new();
    let mut compared = 0;
    for (category, names) in &by_category {
        for (i, a) in names.iter().enumerate() {
            for b in &names[i + 1..] {
                compared += 1;
                if probed[*a].1 != probed[*b].1 {
                    continue;
                }
                if allowed.contains(&(a.as_str(), b.as_str()))
                    || allowed.contains(&(b.as_str(), a.as_str()))
                {
                    continue;
                }
                collisions.push(format!("{category}: !{a} == !{b}"));
            }
        }
    }
    assert!(compared > 200, "only {compared} pairs were compared");
    assert!(
        collisions.is_empty(),
        "these tags read identically over the probe stream — either one is \
         wired to the other's indicator, or they are a real alias and belong in \
         ALIASES with a reason:\n  {}",
        collisions.join("\n  "),
    );
}

/// An alias that is no longer one is a licence to be wrong: the pair would keep
/// passing after a genuine wiring mistake made them agree for a new reason.
#[test]
fn every_declared_alias_still_reads_identically() {
    let probed = probed();
    let stale: Vec<&str> = ALIASES
        .iter()
        .filter(|(a, b, _)| match (probed.get(*a), probed.get(*b)) {
            (Some(x), Some(y)) => x != y,
            _ => true,
        })
        .map(|(a, _, _)| *a)
        .collect();
    assert!(
        stale.is_empty(),
        "ALIASES claims these pairs are the same function, but they now read \
         differently — drop the entry: {stale:?}",
    );
}

/// **A banded family is ordered.** `lower <= middle <= upper`, every bar,
/// which pins an upper/lower swap without depending on the sibling tag.
#[test]
fn every_banded_family_reads_in_order() {
    let probed = probed();
    let numeric = |name: &str| -> Vec<Option<Real>> {
        probed
            .get(name)
            .unwrap_or_else(|| panic!("!{name} was not probed"))
            .iter()
            .map(|v| {
                v.as_ref()
                    .map(|s| s.trim_start_matches('r').parse().expect("a real reading"))
            })
            .collect()
    };

    for family in ["bb", "keltner", "donchian"] {
        let lower = numeric(&format!("{family}_lower"));
        let middle = numeric(&format!("{family}_middle"));
        let upper = numeric(&format!("{family}_upper"));
        let mut compared = 0;
        for (bar, ((l, m), u)) in lower.iter().zip(&middle).zip(&upper).enumerate() {
            let (Some(l), Some(m), Some(u)) = (l, m, u) else {
                continue;
            };
            assert!(
                l <= m && m <= u,
                "{family} bands are out of order on bar {bar}: \
                 lower {l}, middle {m}, upper {u}"
            );
            compared += 1;
        }
        assert!(compared > 0, "no {family} bar had all three bands");
    }
}
