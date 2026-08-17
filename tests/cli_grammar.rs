//! End-to-end tests of the `fugazi grammar` / `fugazi schema` binary commands —
//! the CLI face of the `spec::grammar` introspection. They assert the commands
//! emit *only* JSON on stdout (so the output pipes into `jq` / a consumer), and
//! that the shape matches the library functions the Python bindings expose.

use std::process::Command;

/// Run a subcommand, assert success, and parse stdout as JSON.
fn run_json(args: &[&str]) -> serde_json::Value {
    let out = Command::new(env!("CARGO_BIN_EXE_fugazi"))
        .args(args)
        .output()
        .expect("spawn fugazi");
    assert!(
        out.status.success(),
        "`fugazi {}` failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "`fugazi {}` stdout is not valid JSON: {e}\n{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stdout)
        )
    })
}

#[test]
fn grammar_emits_the_descriptor_document() {
    let doc = run_json(&["grammar"]);
    assert_eq!(doc["schema_version"], 4, "schema_version");
    let tags = doc["tags"].as_array().expect("tags is an array");
    assert!(!tags.is_empty(), "tags non-empty");

    // Every document-level group is present — the whole point of surfacing the
    // descriptor to CLI-only consumers.
    let groups: std::collections::BTreeSet<&str> =
        tags.iter().filter_map(|t| t["group"].as_str()).collect();
    for want in ["node", "selection", "universe", "weighting", "document"] {
        assert!(groups.contains(want), "missing group {want}; got {groups:?}");
    }

    // A record carries the full contract shape.
    let first = &tags[0];
    for key in [
        "name", "group", "kind", "shape", "fields", "output", "projections", "payload",
        "category", "doc", "since",
    ] {
        assert!(first.get(key).is_some(), "record missing key {key}");
    }

    // The v4 addition: an expression slot says what it must be filled with, so
    // a consumer can offer only the tags whose `output` matches.
    let and = tags
        .iter()
        .find(|t| t["name"] == "and")
        .expect("!and is a node tag");
    let lhs = and["fields"]
        .as_array()
        .expect("fields")
        .iter()
        .find(|f| f["name"] == "lhs")
        .expect("!and has an lhs");
    assert_eq!(lhs["node_output"], serde_json::json!(["bool"]), "!and lhs");
    // Omitted, not null, on a slot that holds no free expression — so a v3
    // consumer sees the record it always saw.
    let sma = tags
        .iter()
        .find(|t| t["name"] == "sma")
        .expect("!sma is a node tag");
    let period = sma["fields"]
        .as_array()
        .expect("fields")
        .iter()
        .find(|f| f["name"] == "period")
        .expect("!sma has a period");
    assert!(period.get("node_output").is_none(), "scalar field unstamped");
}

#[test]
fn schema_emits_the_expression_schema() {
    let schema = run_json(&["schema"]);
    assert_eq!(
        schema["$schema"], "https://json-schema.org/draft/2020-12/schema",
        "draft 2020-12"
    );
    assert_eq!(schema["$ref"], "#/$defs/node", "root $ref");
    assert!(schema["$defs"]["node"].is_object(), "node def present");
}

#[test]
fn schema_document_emits_the_five_shapes() {
    let schema = run_json(&["schema", "--document"]);
    let one_of = schema["oneOf"].as_array().expect("document root is a oneOf");
    assert_eq!(one_of.len(), 5, "single/pairs/basket/multi/portfolio");
}
