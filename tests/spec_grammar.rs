//! The grammar descriptor (`spec_grammar()`) must stay a faithful, complete,
//! JSON-serializable reflection of the serde tag vocabulary. These tests are
//! the guard: they pin the derived `name` set against serde's own variant list
//! (so the derive's name algorithm can never silently diverge), and check that
//! every record is well-formed.

use std::collections::BTreeSet;

use fugazi::spec::grammar::{
    GrammarDefault, GrammarField, SCHEMA_VERSION, spec_grammar, spec_grammar_document,
};
use fugazi::spec::typecheck::{REWRITTEN_TAGS, known_node_tags, known_selection_tags};

/// The one authority for *names* is serde's variant list. The descriptor's
/// names, per group, must equal it exactly — this is what lets `spec_tags()`
/// be a projection of `spec_grammar()` with unchanged output.
#[test]
fn names_match_serde_variant_list() {
    let grammar = spec_grammar();

    let node: BTreeSet<&str> = grammar
        .iter()
        .filter(|t| t.group == "node")
        .map(|t| t.name.as_str())
        .collect();
    let selection: BTreeSet<&str> = grammar
        .iter()
        .filter(|t| t.group == "selection")
        .map(|t| t.name.as_str())
        .collect();

    let want_node: BTreeSet<String> = known_node_tags().into_iter().collect();
    let want_selection: BTreeSet<String> = known_selection_tags().into_iter().collect();

    let want_node: BTreeSet<&str> = want_node.iter().map(|s| s.as_ref()).collect();
    let want_selection: BTreeSet<&str> = want_selection.iter().map(|s| s.as_ref()).collect();

    assert_eq!(node, want_node, "node tag names drifted from serde");
    assert_eq!(
        selection, want_selection,
        "selection tag names drifted from serde"
    );
}

/// The document-level groups the derive can't reach. `universe` is reflected
/// off `UniverseSpec`, but `weighting` / `document` are hand-authored (their
/// tags never reach the typed parse), so their name set is pinned here to the
/// parser's own rewrite list — a new load-time tag can't ship without a row.
#[test]
fn document_level_groups_are_pinned() {
    let grammar = spec_grammar();
    let group_names = |g: &str| -> BTreeSet<String> {
        grammar
            .iter()
            .filter(|t| t.group == g)
            .map(|t| t.name.clone())
            .collect()
    };

    // `universe` mirrors the two `UniverseSpec` variants exactly.
    assert_eq!(
        group_names("universe"),
        BTreeSet::from(["all_of".to_string(), "any_of".to_string()]),
        "universe group drifted from UniverseSpec",
    );

    // `weighting` ∪ `document` == the parser's rewrite tags ∪ `fixed` (the one
    // weights-sugar tag that isn't a load-time placeholder). If a tag joins
    // `REWRITTEN_TAGS`, it must gain a descriptor row here.
    let hand_authored: BTreeSet<String> = group_names("weighting")
        .union(&group_names("document"))
        .cloned()
        .collect();
    let mut want: BTreeSet<String> = REWRITTEN_TAGS.iter().map(|s| s.to_string()).collect();
    want.insert("fixed".to_string());
    assert_eq!(
        hand_authored, want,
        "hand-authored weighting/document tags drifted from REWRITTEN_TAGS ∪ {{fixed}} — \
         add or remove a row in grammar::document_grammar_tags",
    );
}

/// Every record is classified and drawn from the closed legend vocabularies.
#[test]
fn every_tag_is_well_formed() {
    const KINDS: &[&str] = &[
        "source",
        "indicator",
        "operator",
        "predicate",
        "function",
        "selection",
        // Document-level families (universe declaration, portfolio weight sugar,
        // load-time composition/substitution).
        "universe",
        "weighting",
        "document",
    ];
    const SHAPES: &[&str] = &["unit", "newtype", "seq", "map"];
    // Every narrowing a form can declare. Absent means "wherever the group is
    // accepted", which is the case for all ~150 expression tags.
    const SCOPES: &[&str] = &["template", "portfolio_weights", "internal"];
    const OUTPUTS: &[&str] = &[
        "scalar",
        "bool",
        "str",
        "time",
        "candle",
        "atom",
        "book",
        "any",
        "selection",
        "struct",
        // A document-level directive that resolves at load, not an expression.
        "none",
    ];
    const FIELD_TYPES: &[&str] = &[
        "node",
        "node_list",
        "str_list",
        "symbol_list",
        "number_list",
        "uint",
        "positive_uint",
        "number",
        "str",
        // The two refinements of `str`. A field wears one by being typed
        // `SymbolName` / `FreqToken` rather than `String` — so this list grows
        // only when a *type* does, which is the guard doing its job.
        "symbol",
        "frequency",
        "bool",
        "strategy",
        "match_cases",
        "str_operand",
        "selection",
        "literal",
        "other",
    ];

    for tag in spec_grammar() {
        assert!(!tag.name.is_empty(), "empty tag name");
        assert!(
            !tag.category.trim().is_empty(),
            "!{}: empty category",
            tag.name
        );
        assert!(
            KINDS.contains(&tag.kind.as_str()),
            "!{}: bad kind {}",
            tag.name,
            tag.kind
        );
        assert!(
            OUTPUTS.contains(&tag.output.as_str()),
            "!{}: bad output {}",
            tag.name,
            tag.output
        );

        // Every tag has at least a canonical form, and no two forms share a
        // shape — two `map` spellings of one tag would be indistinguishable to
        // a consumer, and nothing in the parser produces that.
        assert!(!tag.forms.is_empty(), "!{}: no forms", tag.name);
        let shapes: BTreeSet<&str> = tag.forms.iter().map(|f| f.shape.as_str()).collect();
        assert_eq!(
            shapes.len(),
            tag.forms.len(),
            "!{}: two forms share a shape",
            tag.name
        );

        for (i, form) in tag.forms.iter().enumerate() {
            let at = format!("!{} form[{i}]", tag.name);
            assert!(
                SHAPES.contains(&form.shape.as_str()),
                "{at}: bad shape {}",
                form.shape
            );
            if let Some(scope) = form.scope.as_deref() {
                assert!(SCOPES.contains(&scope), "{at}: bad scope {scope}");
            }
            // Only `map` forms carry fields; only `newtype`/`seq` carry a payload.
            if form.shape != "map" {
                assert!(form.fields.is_empty(), "{at}: non-map form has fields");
            }
            match form.shape.as_str() {
                "newtype" | "seq" => assert!(
                    form.payload
                        .as_deref()
                        .is_some_and(|p| FIELD_TYPES.contains(&p)),
                    "{at}: {} form needs a known payload type, got {:?}",
                    form.shape,
                    form.payload
                ),
                _ => assert!(
                    form.payload.is_none(),
                    "{at}: {} form has a payload",
                    form.shape
                ),
            }
            // An alternate exists because it does something the canonical form
            // can't; a consumer offering it has to be able to say what.
            if i > 0 {
                assert!(
                    form.doc.as_deref().unwrap_or("").trim().len() > 20,
                    "{at}: a non-canonical form needs a `doc` explaining what it \
                     does that the canonical spelling cannot",
                );
            }
            for f in &form.fields {
                assert!(
                    FIELD_TYPES.contains(&f.ty.as_str()),
                    "{at}.{}: bad field type {}",
                    f.name,
                    f.ty
                );
                // A required field never carries a default; an optional one may.
                if f.required {
                    assert!(
                        f.default.is_none(),
                        "{at}.{}: required field has a default",
                        f.name
                    );
                }
            }
        }
    }
}

/// Documentation is self-maintaining: every tag and every field must carry
/// prose, so a new one cannot ship undocumented and silently degrade the
/// generated reference. This is the CI gate behind the 0.49 prose backfill.
#[test]
fn every_tag_and_field_is_documented() {
    let mut missing_tags = Vec::new();
    let mut missing_fields = Vec::new();
    for tag in spec_grammar() {
        if tag.doc.as_deref().unwrap_or("").trim().is_empty() {
            missing_tags.push(tag.name.clone());
        }
        for f in tag.forms.iter().flat_map(|form| &form.fields) {
            if f.doc.as_deref().unwrap_or("").trim().is_empty() {
                missing_fields.push(format!("{}.{}", tag.name, f.name));
            }
        }
    }
    assert!(
        missing_tags.is_empty(),
        "these tags have no `///` doc — add one next to the variant:\n  {}",
        missing_tags.join("\n  ")
    );
    assert!(
        missing_fields.is_empty(),
        "these fields have no `///` doc — add one next to the field:\n  {}",
        missing_fields.join("\n  ")
    );

    // No doc-source markup in the prose that ships to consumers: neither a `{{`
    // template artifact nor rustdoc/markdown link syntax (`[`Type`]`, `](url)`),
    // which the derive strips so the descriptor reads as presentation text.
    let mut artifacts = Vec::new();
    for tag in spec_grammar() {
        let form_docs = tag
            .forms
            .iter()
            .flat_map(|form| std::iter::once(&form.doc).chain(form.fields.iter().map(|f| &f.doc)));
        for doc in std::iter::once(&tag.doc).chain(form_docs) {
            let d = doc.as_deref().unwrap_or("");
            if d.contains("{{") || d.contains("[`") || d.contains("](") {
                artifacts.push(tag.name.clone());
            }
        }
    }
    assert!(
        artifacts.is_empty(),
        "these tags' prose contains template or rustdoc-link markup: {artifacts:?}"
    );
}

/// The whole document, including native `serde_json::Value` defaults, must be
/// JSON-serializable — that's the contract for the Python / web consumers.
#[test]
fn document_is_json_serializable() {
    let doc = spec_grammar_document();
    let text = serde_json::to_string(&doc).expect("descriptor serializes to JSON");
    assert!(text.contains("\"schema_version\""));
    assert_eq!(doc["schema_version"], SCHEMA_VERSION);
    assert!(doc["tags"].as_array().is_some_and(|a| !a.is_empty()));
}

/// Spot-check the reflected metadata against known definitions: field
/// types/optionality from serde, and the const-backed numeric defaults.
#[test]
fn reflects_fields_and_defaults() {
    let grammar = spec_grammar();
    let by_name = |n: &str| grammar.iter().find(|t| t.name == n).expect(n);

    let sma = by_name("sma");
    assert_eq!(sma.canonical().shape, "map");
    assert_eq!(sma.forms.len(), 1, "!sma has one spelling");
    assert_eq!(sma.output, "scalar");
    let src = sma
        .canonical()
        .fields
        .iter()
        .find(|f| f.name == "source")
        .unwrap();
    assert_eq!(src.ty, "node");
    assert!(!src.required, "sma.source has a default -> optional");
    assert!(
        src.default
            .as_ref()
            .and_then(GrammarDefault::literal)
            .is_none(),
        "a node default is not a JSON literal — that is why `default` is tagged",
    );
    assert_eq!(
        default_fragment(src),
        Some("!close"),
        "omitting !sma's source is writing !close, and the descriptor has to say so",
    );

    // The other three node defaults, one per shape of fallback.
    let field = |tag: &str, name: &str| {
        by_name(tag)
            .canonical()
            .fields
            .iter()
            .find(|f| f.name == name)
            .unwrap_or_else(|| panic!("!{tag}.{name}"))
            .clone()
    };
    assert_eq!(default_fragment(&field("atr", "source")), Some("!current"));
    assert_eq!(
        default_fragment(&field("donchian_upper", "high")),
        Some("!high")
    );
    assert_eq!(
        default_fragment(&field("donchian_upper", "low")),
        Some("!low")
    );
    assert_eq!(
        default_fragment(&field("top_bottom", "of")),
        Some("!everything")
    );

    // ... and the case that must stay silent: a candle leaf's `source:` defaults
    // to the strategy's own series, which no tag spells. `null` + `null` is the
    // honest answer, and inventing one here would be worse than prose.
    let own_series = field("close", "source");
    assert!(!own_series.required);
    assert!(
        own_series.default.is_none(),
        "!close.source has no expressible default — reporting one would be a lie",
    );

    // Same answer, same reason, one type over: an `Option` *scalar*. The old
    // untagged `default` reported these as `Some(Value::Null)`, which
    // serialised to the same `null` a real absence did.
    let freq = field("pick", "freq");
    assert!(
        !freq.required && freq.default.is_none(),
        "!pick.freq defaults to nothing"
    );
    let period = sma
        .canonical()
        .fields
        .iter()
        .find(|f| f.name == "period")
        .unwrap();
    // A period is a `NonZeroUsize`, which the descriptor reports as
    // `positive_uint` so the generated JSON schema can say `minimum: 1`.
    assert_eq!(period.ty, "positive_uint");
    assert!(period.required, "sma.period has no default");

    // Const-backed defaults surface as literals.
    let macd = by_name("macd_line");
    let fast = macd
        .canonical()
        .fields
        .iter()
        .find(|f| f.name == "fast")
        .unwrap();
    assert!(!fast.required);
    assert_eq!(literal(fast), Some(serde_json::json!(12)));
    let bb = by_name("bb_upper");
    let k = bb
        .canonical()
        .fields
        .iter()
        .find(|f| f.name == "k")
        .unwrap();
    assert_eq!(literal(k), Some(serde_json::json!(2.0)));
    // The two cases are exclusive by construction — `default` is one tagged
    // value, so a literal cannot also carry a fragment.
    assert!(default_fragment(fast).is_none() && default_fragment(k).is_none());

    // A bool predicate and its optional epsilon.
    let gt = by_name("gt");
    assert_eq!(gt.kind, "predicate");
    assert_eq!(gt.output, "bool");
    let eps = gt
        .canonical()
        .fields
        .iter()
        .find(|f| f.name == "epsilon")
        .unwrap();
    assert!(
        !eps.required,
        "Option field is optional even without serde default"
    );
}

/// **A `default_expr` must be equivalent to omitting the field.** The claim the
/// descriptor makes for 69 slots — that `!ema { period: 10 }` and
/// `!ema { source: !close, period: 10 }` are the same expression — settled
/// against the parser rather than asserted in prose.
///
/// Both spellings are parsed and their trees compared, so this catches the
/// three ways the claim could go wrong: a fragment that doesn't parse at all, a
/// fragment that parses to a *different* node than the default (a renamed leaf,
/// a `default_source` that stops returning `!close`), and a slot that isn't
/// actually omissible. It is the reason `default_expr` is reflected off the
/// default's own value instead of scraped out of the field's prose — prose is a
/// claim nothing can check, and this is the check.
#[test]
fn a_default_expr_is_equivalent_to_omitting_the_field() {
    let mut checked = 0;
    let mut broken = Vec::new();
    for tag in spec_grammar() {
        for form in &tag.forms {
            let Some(base) = minimal_body(form) else {
                continue;
            };
            for f in form.fields.iter().filter(|f| default_fragment(f).is_some()) {
                let expr = default_fragment(f).expect("filtered");
                let at = format!("!{}.{}", tag.name, f.name);
                // The fragment is YAML the way a document writes it, so it goes
                // through the loader's own YAML → JSON-bridge normalisation.
                let fragment = match fugazi::spec::input::parse_value(expr) {
                    Ok(v) => v,
                    Err(e) => {
                        broken.push(format!("{at}: default_expr {expr:?} is not YAML: {e}"));
                        continue;
                    }
                };
                let mut written = base.clone();
                written.insert(f.name.clone(), fragment);
                let omitted = tagged(&tag.name, base.clone());
                let written = tagged(&tag.name, written);
                match (
                    parse_tree(&tag.group, &omitted),
                    parse_tree(&tag.group, &written),
                ) {
                    (Ok(a), Ok(b)) if a == b => checked += 1,
                    (Ok(a), Ok(b)) => broken.push(format!(
                        "{at}: omitting the field is not `{expr}`\n      omitted: {a}\n      \
                         written: {b}"
                    )),
                    (Err(e), _) => {
                        broken.push(format!("{at}: the field is not omissible: {omitted} — {e}"))
                    }
                    (_, Err(e)) => {
                        broken.push(format!("{at}: {expr} does not parse in the slot: {e}"))
                    }
                }
            }
        }
    }
    assert!(
        broken.is_empty(),
        "the descriptor claims these defaults, and the parser disagrees:\n  {}",
        broken.join("\n  "),
    );
    // A floor, not a count: the point is that the walk reached the slots at all.
    // Every wrapped indicator's `source` is one, so this cannot thin out
    // quietly.
    assert!(
        checked >= 60,
        "only {checked} default_expr claims were reachable to check"
    );
}

/// **A defaulted expression slot must name what it defaults to.** The converse
/// guard: a slot whose default is a node has to report the fragment, not leave
/// it in English.
///
/// The derive fills `default_expr` for every non-`Option` defaulted node field,
/// so the way this fails is `grammar::default_expr_of` meeting a default it
/// cannot spell (a `!value 0`, a list) and answering `None` — which would ship
/// as an indistinguishable `null`. Teach it the spelling; don't demote the fact
/// to prose.
///
/// Two exemptions, both structural rather than editorial:
///
/// - a slot demanding `["atom"]` is a **blessed-series re-root** (`!close`'s
///   `source:`), whose absence means "the strategy's own series" — no tag says
///   that, so there is nothing to report;
/// - a slot with **no** demand is not a free expression at all (a *book
///   selector* like `!drawdown`'s `source:`), same story.
///
/// Both would also cover a future defaulted node slot in those positions, which
/// is the limit of what a walk over the descriptor can prove; anything else has
/// to say what it defaults to.
#[test]
fn defaulted_expression_slots_name_their_default() {
    let mut silent = Vec::new();
    for tag in spec_grammar() {
        for form in &tag.forms {
            for f in &form.fields {
                let is_expression = matches!(f.ty.as_str(), "node" | "node_list" | "selection");
                if f.required || f.default.is_some() || !is_expression {
                    continue;
                }
                let demand: Option<Vec<&str>> = f
                    .node_output
                    .as_ref()
                    .map(|d| d.iter().map(String::as_str).collect());
                match demand.as_deref() {
                    // A blessed-series re-root, or a build-time selector.
                    Some(["atom"]) | None => {}
                    _ => silent.push(format!("!{}.{} ({:?})", tag.name, f.name, f.node_output)),
                }
            }
        }
    }
    assert!(
        silent.is_empty(),
        "these expression slots are optional and report no default at all — if omitting \
         one is equivalent to writing a node, `grammar::default_expr_of` has to spell \
         it:\n  {}",
        silent.join("\n  "),
    );
}

/// The YAML fragment a field defaults to, if its default is one.
fn default_fragment(f: &GrammarField) -> Option<&str> {
    f.default.as_ref().and_then(GrammarDefault::expr)
}

/// The JSON literal a field defaults to, if its default is one.
fn literal(f: &GrammarField) -> Option<serde_json::Value> {
    f.default
        .as_ref()
        .and_then(GrammarDefault::literal)
        .cloned()
}

/// The **minimal** JSON-bridge body for a `map` form: required fields filled
/// with a probe, every optional one omitted. `None` for a non-`map` form, or
/// when a required field holds something no probe can fabricate.
///
/// Distinct from [`probe`], which fills expression slots whether or not they're
/// required — here the omitted ones are the subject.
fn minimal_body(
    form: &fugazi::spec::grammar::GrammarForm,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    if form.shape != "map" {
        return None;
    }
    let mut body = serde_json::Map::new();
    for f in form.fields.iter().filter(|f| f.required) {
        body.insert(f.name.clone(), filler(&f.ty)?);
    }
    Some(body)
}

/// `{ "<tag>": { <body> } }` — the JSON bridge encoding of a `map` spelling.
fn tagged(name: &str, body: serde_json::Map<String, serde_json::Value>) -> serde_json::Value {
    serde_json::json!({ name: serde_json::Value::Object(body) })
}

/// Parse a JSON-bridge document in its group's vocabulary, rendering the tree
/// as its `Debug` — structural equality without needing `PartialEq` on either
/// spec enum.
fn parse_tree(group: &str, doc: &serde_json::Value) -> Result<String, String> {
    match group {
        "selection" => serde_json::from_value::<fugazi::spec::SelectionRuleSpec>(doc.clone())
            .map(|v| format!("{v:?}"))
            .map_err(|e| e.to_string()),
        _ => serde_json::from_value::<fugazi::spec::NodeSpec>(doc.clone())
            .map(|v| format!("{v:?}"))
            .map_err(|e| e.to_string()),
    }
}

/// Every tag in the vocabulary must appear in `docs/STRATEGIES.md`, the
/// user-facing tag reference.
///
/// This is the one "add an indicator" step that used to be enforced by nothing
/// — `spec_grammar::tests::every_tag_and_field_is_documented` covers the `///`
/// prose that feeds `fugazi list indicators` and `fugazi schema`, so the
/// *machine-readable* reference could never go stale, but the prose reference
/// silently could. It had: 15 tags were missing, including the whole book-field
/// family and all five embedded-strategy metrics.
///
/// A tag counts as documented if it appears as `!name` or as a `name` code
/// span — the candle-field and position-anchored leaves are written as bare
/// words in the doc because that is how they are written in a document.
#[test]
fn every_tag_appears_in_the_strategies_reference() {
    let doc = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/STRATEGIES.md"),
    )
    .expect("docs/STRATEGIES.md must be readable");

    let grammar = spec_grammar();
    let missing: Vec<&str> = grammar
        .iter()
        .filter(|t| t.group == "node" || t.group == "selection")
        .map(|t| t.name.as_str())
        .filter(|name| !doc.contains(&format!("!{name}")) && !doc.contains(&format!("`{name}`")))
        .collect();

    assert!(
        missing.is_empty(),
        "docs/STRATEGIES.md documents no `!{}`{}. \
         Every tag needs a line in the reference — see docs/CONTRIBUTING.md step 9.",
        missing.join("`, no `!"),
        if missing.len() > 1 {
            format!(" ({} tags)", missing.len())
        } else {
            String::new()
        },
    );
}

/// Build a probe document for one form of `tag` in the JSON bridge encoding,
/// or `None` when this form holds something no probe can fabricate (an
/// embedded strategy document).
///
/// Expression slots are filled with `!get { key: probe }`, whose output type is
/// schema-dependent and therefore admitted everywhere — the probe has to
/// *parse*, not to typecheck.
fn probe(name: &str, form: &fugazi::spec::grammar::GrammarForm) -> Option<serde_json::Value> {
    use serde_json::json;
    let body = match form.shape.as_str() {
        "unit" => serde_json::Value::Null,
        "newtype" | "seq" => filler(form.payload.as_deref()?)?,
        "map" => {
            let mut body = serde_json::Map::new();
            for f in &form.fields {
                // Optional non-expression keys are omitted — the point is the
                // minimal document, and a default is not this test's business.
                let is_node = matches!(f.ty.as_str(), "node" | "node_list" | "match_cases");
                if !f.required && !is_node {
                    continue;
                }
                body.insert(f.name.clone(), filler(&f.ty)?);
            }
            serde_json::Value::Object(body)
        }
        other => panic!("!{name}: unknown shape {other}"),
    };
    Some(json!({ name: body }))
}

/// A stand-in value of grammar type `ty`, in the JSON bridge encoding.
///
/// Expression slots get `!get { key: probe }`, whose output type is
/// schema-dependent and therefore admitted everywhere — a probe has to *parse*,
/// not to typecheck. `None` for `strategy`, a whole embedded document.
fn filler(ty: &str) -> Option<serde_json::Value> {
    use serde_json::json;
    Some(match ty {
        "node" => json!({ "get": { "key": "probe" } }),
        "node_list" => json!([{ "get": { "key": "probe" } }]),
        "match_cases" => json!([{ "when": 1, "value": { "get": { "key": "probe" } } }]),
        "positive_uint" | "uint" | "number" | "literal" => json!(1),
        "str" | "str_operand" => json!("probe"),
        "str_list" => json!(["PROBE"]),
        "number_list" => json!([1.0]),
        "bool" => json!(true),
        // `strategy` — a whole embedded document, out of reach here.
        _ => return None,
    })
}

/// Whether a JSON-bridge document parses as an expression.
fn parses(doc: &serde_json::Value) -> bool {
    serde_json::from_value::<fugazi::spec::NodeSpec>(doc.clone()).is_ok()
}

/// **A declared form must actually parse.** The descriptor claims a set of
/// spellings per tag; this runs each one through the real parser.
///
/// Without this, `forms` is prose: an alternate could be declared for a
/// spelling the parser never took, or survive a refactor that removed it, and
/// the only symptom would be a downstream tool generating documents fugazi
/// rejects. With it, the reflected tier keeps the guarantee the module docs
/// claim — the derive reads the variant, the attribute declares what the
/// variant can't express, and the parser is what settles both.
#[test]
fn every_declared_form_parses() {
    let mut broken = Vec::new();
    for tag in spec_grammar() {
        // Only the expression vocabularies parse as a `NodeSpec`; the
        // document-level groups are exercised by `document_forms_resolve`.
        if tag.group != "node" {
            continue;
        }
        for (i, form) in tag.forms.iter().enumerate() {
            let Some(doc) = probe(&tag.name, form) else {
                continue;
            };
            if !parses(&doc) {
                broken.push(format!("!{} form[{i}] ({}): {doc}", tag.name, form.shape));
            }
        }
    }
    assert!(
        broken.is_empty(),
        "the descriptor declares these spellings, but the parser rejects them:\n  {}",
        broken.join("\n  "),
    );
}

/// A `map` tag with no required key parses from an **explicit null** body, not
/// only from the bare string — `{"close": null}` is what a YAML `!close`
/// normalises to on the way in.
///
/// The parser half of `test_all_optional_map_tags_accept_an_explicit_null_body`
/// in the Python suite. `spec_json_schema()` used to reject this shape while the
/// parser took it, so the schema called a document invalid that fugazi loads —
/// and, once a field advertised a `default`, its own advertised value.
#[test]
fn all_optional_map_tags_parse_from_a_null_body() {
    let mut rejected = Vec::new();
    let mut checked = 0;
    for tag in spec_grammar() {
        if tag.group != "node" {
            continue;
        }
        for form in &tag.forms {
            if form.shape != "map" || form.fields.iter().any(|f| f.required) {
                continue;
            }
            checked += 1;
            if !parses(&serde_json::json!({ &tag.name: serde_json::Value::Null })) {
                rejected.push(tag.name.clone());
            }
        }
    }
    assert!(
        rejected.is_empty(),
        "these tags refuse an explicit null body, which the schema advertises as \
         valid: {rejected:?}",
    );
    assert!(
        checked >= 30,
        "only {checked} all-optional map forms were reachable"
    );
}

/// **A form the parser accepts must be declared.** The converse guard, and the
/// one that would have caught the v4 bug.
///
/// The undeclared spellings were all one pattern — a one-slot wrapper taking
/// its inner either bare or under a lone `source:` key, which
/// `expr::extract_edge_inner` implements — so that is the pattern probed here:
/// for every tag shaped like a unary wrapper, the *mirror* spelling must parse
/// exactly when a form declares it. `!changed` / `!became_true` /
/// `!became_false` / `!unstable` declare it and do; `!not`, `!close`, and the
/// other unary-looking tags don't and must not.
#[test]
fn no_unary_wrapper_hides_an_undeclared_mirror() {
    let mut undeclared = Vec::new();
    for tag in spec_grammar() {
        if tag.group != "node" {
            continue;
        }
        let canonical = tag.canonical();
        // The mirror of the canonical spelling, when the tag is shaped like a
        // unary wrapper at all.
        let mirror = match canonical.shape.as_str() {
            "newtype" if canonical.payload.as_deref() == Some("node") => {
                serde_json::json!({ &tag.name: { "source": { "get": { "key": "probe" } } } })
            }
            "map" if canonical.fields.iter().all(|f| f.name == "source") => {
                serde_json::json!({ &tag.name: { "get": { "key": "probe" } } })
            }
            _ => continue,
        };
        let declared = tag.forms.len() > 1;
        if parses(&mirror) && !declared {
            undeclared.push(format!("!{}: {mirror}", tag.name));
        }
    }
    assert!(
        undeclared.is_empty(),
        "the parser takes these spellings, but no `forms` entry declares them — add \
         `#[grammar(alt = \"unary_source\")]` to the variant:\n  {}",
        undeclared.join("\n  "),
    );
}

/// The hand-authored `document` / `weighting` rows, exercised through the
/// passes that actually resolve them.
///
/// These tags never reach the typed parse, so `every_declared_form_parses`
/// cannot see them — and they are the rows most able to drift, being the one
/// part of the descriptor written by hand. Each declared form is run through
/// its own pass and must resolve.
#[test]
fn document_forms_resolve() {
    use std::collections::HashMap;

    let grammar = spec_grammar();
    let forms_of = |name: &str| {
        grammar
            .iter()
            .find(|t| t.name == name)
            .unwrap_or_else(|| panic!("!{name} has a descriptor row"))
            .forms
            .clone()
    };

    // `!param` — bare string and `{ key, default }`, the second being the only
    // one that can carry a fallback.
    let param = forms_of("param");
    assert_eq!(param.len(), 2, "!param has two spellings");
    let table = HashMap::from([("SET".to_string(), serde_json::json!(3))]);
    for (doc, want) in [
        (serde_json::json!({ "param": "SET" }), serde_json::json!(3)),
        (
            serde_json::json!({ "param": { "key": "SET" } }),
            serde_json::json!(3),
        ),
        (
            serde_json::json!({ "param": { "key": "UNSET", "default": 8 } }),
            serde_json::json!(8),
        ),
    ] {
        let got = fugazi::spec::params::substitute(doc.clone(), &table)
            .unwrap_or_else(|e| panic!("!param spelling {doc} must resolve: {e}"));
        assert_eq!(got, want, "!param {doc}");
    }
    // The bare form cannot carry a default — which is the whole reason the map
    // form is declared, so pin it rather than leaving it to the prose.
    assert!(
        fugazi::spec::params::substitute(serde_json::json!({ "param": "UNSET" }), &HashMap::new())
            .is_err(),
        "the bare spelling has nowhere to put a default, so an unset key is an error",
    );

    // `!arg` — the same two spellings, resolved by the build-time twin.
    assert_eq!(forms_of("arg").len(), 2, "!arg has two spellings");
    let args = HashMap::from([("SYM".to_string(), serde_json::json!("BTC"))]);
    for (doc, want) in [
        (
            serde_json::json!({ "arg": "SYM" }),
            serde_json::json!("BTC"),
        ),
        (
            serde_json::json!({ "arg": { "key": "SYM" } }),
            serde_json::json!("BTC"),
        ),
        (
            serde_json::json!({ "arg": { "key": "OTHER", "default": "ETH" } }),
            serde_json::json!("ETH"),
        ),
    ] {
        let got = fugazi::spec::args::substitute(doc.clone(), &args)
            .unwrap_or_else(|e| panic!("!arg spelling {doc} must resolve: {e}"));
        assert_eq!(got, want, "!arg {doc}");
    }

    // `!import` — bare path and `{ path, params }`, the second being the only
    // one that can parameterise the imported subtree.
    assert_eq!(forms_of("import").len(), 2, "!import has two spellings");
    let dir = std::env::temp_dir().join("fugazi_grammar_forms");
    std::fs::create_dir_all(&dir).expect("temp dir");
    std::fs::write(
        dir.join("frag.yml"),
        "period: !param { key: N, default: 7 }\n",
    )
    .expect("write fragment");
    let bare =
        fugazi::spec::imports::resolve(serde_json::json!({ "import": "frag.yml" }), &dir, &dir)
            .expect("bare !import resolves");
    assert_eq!(
        bare["period"],
        serde_json::json!({ "param": { "key": "N", "default": 7 } })
    );
    let keyed = fugazi::spec::imports::resolve(
        serde_json::json!({ "import": { "path": "frag.yml", "params": { "N": 21 } } }),
        &dir,
        &dir,
    )
    .expect("keyed !import resolves");
    assert_eq!(
        keyed["period"],
        serde_json::json!(21),
        "inline params are what the keyed spelling exists for",
    );
    let _ = std::fs::remove_dir_all(&dir);

    // `!equal_weight` — the bare portfolio-weights spelling and the sizing
    // `<N>` one, which mean different things and are scoped differently.
    let ew = forms_of("equal_weight");
    assert_eq!(ew.len(), 2, "!equal_weight has two spellings");
    assert_eq!(ew[0].shape, "unit");
    assert_eq!(ew[0].scope.as_deref(), Some("portfolio_weights"));
    assert_eq!(ew[1].shape, "newtype");
    assert_eq!(
        ew[1].scope, None,
        "the sizing spelling goes wherever a node does"
    );
    // The sizing spelling lowers to `!value 1/N` and parses as an expression.
    assert!(
        parses(&serde_json::json!({ "equal_weight": 4 })),
        "!equal_weight <N> is an ordinary node",
    );
}

/// `!arg` is a `document` tag but **not** legal in any position — the one place
/// where reading `group` as a position claim goes wrong, which is why
/// `GrammarForm::scope` exists.
///
/// Nothing substitutes an `!arg` outside a deferred template body, so one
/// written elsewhere reaches the typed parse verbatim and fails. Pin both
/// halves: the descriptor says `template`, and the parser agrees.
#[test]
fn arg_is_scoped_to_templates_and_param_is_not() {
    let grammar = spec_grammar();
    let scopes = |name: &str| -> BTreeSet<Option<String>> {
        grammar
            .iter()
            .find(|t| t.name == name)
            .unwrap_or_else(|| panic!("!{name} has a descriptor row"))
            .forms
            .iter()
            .map(|f| f.scope.clone())
            .collect()
    };
    assert_eq!(
        scopes("arg"),
        BTreeSet::from([Some("template".to_string())]),
        "every !arg spelling is template-scoped",
    );
    assert_eq!(
        scopes("param"),
        BTreeSet::from([None]),
        "!param is position-free — its pass rewrites any value position",
    );

    // The parser's half of the same claim, in a non-template slot.
    let with = |placeholder: &str| {
        format!(
            "root: BTC\nlong:\n  enter: !gt\n    lhs: !sma\n      source: !close\n      \
             period: {placeholder}\n    rhs: !value 1\n"
        )
    };
    let load = |text: &str| {
        fugazi::spec::SingleStrategySpec::from_text_with_params_in(
            text,
            &std::collections::HashMap::new(),
            std::path::Path::new("."),
            std::path::Path::new("."),
            "probe",
        )
    };
    assert!(
        load(&with("!param { key: N, default: 5 }")).is_ok(),
        "!param resolves in a scalar slot of an ordinary document",
    );
    assert!(
        load(&with("!arg N")).is_err(),
        "!arg outside a template has no pass to resolve it — a tool that offers it \
         wherever !param goes emits documents that do not load",
    );
}
