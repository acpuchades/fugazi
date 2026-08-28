//! `spec_json_schema()` must stay a faithful, self-consistent JSON Schema over
//! the same tag vocabulary as `spec_grammar()`. These are the dependency-free
//! structural guards (every `$ref` resolves; every tag is covered exactly once);
//! actual instance validation against a real JSON Schema engine lives in the
//! Python suite (`test_spec_json_schema.py`).

use std::collections::BTreeSet;

use serde_json::Value;

use fugazi::spec::grammar::{spec_document_json_schema, spec_grammar, spec_json_schema};
use fugazi::spec::typecheck::REWRITTEN_TAGS;

/// Collect every `$ref` target string in the document.
fn collect_refs(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(map) => {
            for (k, val) in map {
                if k == "$ref" {
                    if let Value::String(s) = val {
                        out.push(s.clone());
                    }
                } else {
                    collect_refs(val, out);
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|i| collect_refs(i, out)),
        _ => {}
    }
}

/// The tag name(s) a branch covers — the `const` of a bare form, or the single
/// required key of a `{tag: body}` form.
///
/// Recurses through both nested unions: `oneOf` for the bare-vs-keyed pair of a
/// single spelling, `anyOf` for a tag written more than one way (`!changed
/// <node>` and `!changed { source }` are one tag, two branches).
fn branch_names(v: &Value) -> Vec<String> {
    for key in ["oneOf", "anyOf"] {
        if let Some(Value::Array(alts)) = v.get(key) {
            return alts.iter().flat_map(branch_names).collect();
        }
    }
    if let Some(Value::String(c)) = v.get("const") {
        return vec![c.clone()];
    }
    // Single-key tag object: exactly one required property, the tag name.
    if let Some(Value::Array(req)) = v.get("required")
        && req.len() == 1
        && let Some(Value::String(name)) = req.first()
    {
        return vec![name.clone()];
    }
    Vec::new()
}

#[test]
fn root_refs_node() {
    let schema = spec_json_schema();
    assert_eq!(schema["$ref"], "#/$defs/node");
    assert_eq!(
        schema["$schema"],
        "https://json-schema.org/draft/2020-12/schema"
    );
}

#[test]
fn every_ref_resolves() {
    let schema = spec_json_schema();
    let defs = schema["$defs"].as_object().expect("$defs object");
    let mut refs = Vec::new();
    collect_refs(&schema, &mut refs);
    assert!(!refs.is_empty(), "schema has no $refs at all");
    for r in refs {
        let name = r
            .strip_prefix("#/$defs/")
            .unwrap_or_else(|| panic!("odd $ref {r}"));
        assert!(defs.contains_key(name), "$ref {r} points at a missing def");
    }
    for expected in ["node", "selection", "match_case", "strategy"] {
        assert!(defs.contains_key(expected), "missing $defs/{expected}");
    }
}

#[test]
fn node_and_selection_cover_every_tag_exactly() {
    let schema = spec_json_schema();
    let grammar = spec_grammar();

    let want = |group: &str| -> BTreeSet<String> {
        grammar
            .iter()
            .filter(|t| t.group == group)
            .map(|t| t.name.clone())
            .collect()
    };

    // Selection covers exactly its tag set.
    let sel_alts = schema["$defs"]["selection"]["oneOf"]
        .as_array()
        .expect("$defs/selection is a oneOf");
    let sel_covered: BTreeSet<String> = sel_alts.iter().flat_map(branch_names).collect();
    assert_eq!(sel_covered, want("selection"), "selection tag set");

    // Node covers every node tag plus the authored load-time placeholders
    // (`REWRITTEN_TAGS`), which flow in from the parser's own list.
    let node_alts = schema["$defs"]["node"]["oneOf"]
        .as_array()
        .expect("node oneOf");
    let node_covered: BTreeSet<String> = node_alts.iter().flat_map(branch_names).collect();
    let mut want_node = want("node");
    want_node.extend(REWRITTEN_TAGS.iter().map(|s| s.to_string()));
    assert_eq!(node_covered, want_node, "node tags ∪ placeholders");

    // …and the three bare-literal shorthand branches (number / boolean / array).
    let shorthands = node_alts
        .iter()
        .filter(|b| {
            b.get("type").is_some() && b.get("required").is_none() && b.get("const").is_none()
        })
        .count();
    assert_eq!(
        shorthands, 3,
        "expected number/boolean/array literal shorthands"
    );
}

#[test]
fn document_schema_is_well_formed() {
    let schema = spec_document_json_schema();
    let defs = schema["$defs"].as_object().expect("$defs object");

    // The five shapes plus their slot defs plus the shared expression defs.
    for name in [
        "single",
        "pairs",
        "basket",
        "multi",
        "portfolio",
        "side",
        "basket_side",
        "multi_side",
        "universe",
        "portfolio_child",
        "node",
        "selection",
    ] {
        assert!(defs.contains_key(name), "missing $defs/{name}");
    }

    // Root is an `anyOf` over exactly the five shapes — `anyOf` because a
    // document that omits the optional `root:` is structurally both a `single`
    // and a `multi`, and `oneOf` would reject what both shapes accept.
    let root: BTreeSet<String> = schema["anyOf"]
        .as_array()
        .expect("root anyOf")
        .iter()
        .filter_map(|b| b.get("$ref").and_then(|r| r.as_str()))
        .map(|r| r.trim_start_matches("#/$defs/").to_string())
        .collect();
    assert_eq!(
        root,
        ["single", "pairs", "basket", "multi", "portfolio"]
            .iter()
            .map(|s| s.to_string())
            .collect::<BTreeSet<_>>(),
        "document root must be an anyOf of the five shapes"
    );

    // The single-asset `root:` is optional, and the schema *publishes* what
    // omitting it means. A consumer rendering "defaults to …" reads this;
    // pinning it to `root::default_tree` is what stops the published answer
    // drifting from the one the loader actually splices.
    // Same for a pair's two legs, which default to `LEFT` / `RIGHT` off one
    // shared `FREQ`.
    for (shape, key, root_key) in [
        ("single", "root", fugazi::spec::root::RootKey::ROOT),
        ("pairs", "left", fugazi::spec::root::RootKey::LEFT),
        ("pairs", "right", fugazi::spec::root::RootKey::RIGHT),
    ] {
        let doc = &defs[shape];
        assert!(
            !doc["required"]
                .as_array()
                .expect("required list")
                .iter()
                .any(|k| k == key),
            "`{key}:` is optional on the {shape} shape"
        );
        assert_eq!(
            doc["properties"][key]["default"],
            root_key.default_tree(),
            "the schema's published default for `{key}:` must be the one the loader splices"
        );
    }

    // Every $ref resolves.
    let mut refs = Vec::new();
    collect_refs(&schema, &mut refs);
    for r in refs {
        let name = r
            .strip_prefix("#/$defs/")
            .unwrap_or_else(|| panic!("odd $ref {r}"));
        assert!(defs.contains_key(name), "$ref {r} points at a missing def");
    }
}
