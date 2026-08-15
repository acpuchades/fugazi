//! The grammar descriptor (`spec_grammar()`) must stay a faithful, complete,
//! JSON-serializable reflection of the serde tag vocabulary. These tests are
//! the guard: they pin the derived `name` set against serde's own variant list
//! (so the derive's name algorithm can never silently diverge), and check that
//! every record is well-formed.

use std::collections::BTreeSet;

use fugazi::spec::grammar::{SCHEMA_VERSION, spec_grammar, spec_grammar_document};
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

    let want_node: BTreeSet<&str> = want_node.iter().map(String::as_str).collect();
    let want_selection: BTreeSet<&str> = want_selection.iter().map(String::as_str).collect();

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
        "number_list",
        "uint",
        "number",
        "str",
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
        assert!(KINDS.contains(&tag.kind.as_str()), "!{}: bad kind {}", tag.name, tag.kind);
        assert!(
            SHAPES.contains(&tag.shape.as_str()),
            "!{}: bad shape {}",
            tag.name,
            tag.shape
        );
        assert!(
            OUTPUTS.contains(&tag.output.as_str()),
            "!{}: bad output {}",
            tag.name,
            tag.output
        );
        // Only `map` tags carry fields; only `newtype`/`seq` carry a payload.
        if tag.shape != "map" {
            assert!(tag.fields.is_empty(), "!{}: non-map tag has fields", tag.name);
        }
        match tag.shape.as_str() {
            "newtype" | "seq" => assert!(
                tag.payload.as_deref().is_some_and(|p| FIELD_TYPES.contains(&p)),
                "!{}: {} tag needs a known payload type, got {:?}",
                tag.name,
                tag.shape,
                tag.payload
            ),
            _ => assert!(tag.payload.is_none(), "!{}: {} tag has a payload", tag.name, tag.shape),
        }
        for f in &tag.fields {
            assert!(
                FIELD_TYPES.contains(&f.ty.as_str()),
                "!{}.{}: bad field type {}",
                tag.name,
                f.name,
                f.ty
            );
            // A required field never carries a default; an optional one may.
            if f.required {
                assert!(
                    f.default.is_none(),
                    "!{}.{}: required field has a default",
                    tag.name,
                    f.name
                );
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
        for f in &tag.fields {
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

    // No unrendered template artifacts in the prose that ships to consumers.
    let mut artifacts = Vec::new();
    for tag in spec_grammar() {
        for doc in std::iter::once(&tag.doc).chain(tag.fields.iter().map(|f| &f.doc)) {
            if doc.as_deref().unwrap_or("").contains("{{") {
                artifacts.push(tag.name.clone());
            }
        }
    }
    assert!(
        artifacts.is_empty(),
        "these tags' prose contains a `{{{{` template artifact: {artifacts:?}"
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
    assert_eq!(sma.shape, "map");
    assert_eq!(sma.output, "scalar");
    let src = sma.fields.iter().find(|f| f.name == "source").unwrap();
    assert_eq!(src.ty, "node");
    assert!(!src.required, "sma.source has a default -> optional");
    assert!(src.default.is_none(), "node default is null, not a literal");
    let period = sma.fields.iter().find(|f| f.name == "period").unwrap();
    assert_eq!(period.ty, "uint");
    assert!(period.required, "sma.period has no default");

    // Const-backed defaults surface as literals.
    let macd = by_name("macd_line");
    let fast = macd.fields.iter().find(|f| f.name == "fast").unwrap();
    assert!(!fast.required);
    assert_eq!(fast.default, Some(serde_json::json!(12)));
    let bb = by_name("bb_upper");
    let k = bb.fields.iter().find(|f| f.name == "k").unwrap();
    assert_eq!(k.default, Some(serde_json::json!(2.0)));

    // A bool predicate and its optional epsilon.
    let gt = by_name("gt");
    assert_eq!(gt.kind, "predicate");
    assert_eq!(gt.output, "bool");
    let eps = gt.fields.iter().find(|f| f.name == "epsilon").unwrap();
    assert!(!eps.required, "Option field is optional even without serde default");
}
