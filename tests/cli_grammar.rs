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
    assert_eq!(
        doc["schema_version"],
        fugazi::spec::grammar::SCHEMA_VERSION,
        "schema_version"
    );
    let tags = doc["tags"].as_array().expect("tags is an array");
    assert!(!tags.is_empty(), "tags non-empty");

    // Every document-level group is present — the whole point of surfacing the
    // descriptor to CLI-only consumers.
    let groups: std::collections::BTreeSet<&str> =
        tags.iter().filter_map(|t| t["group"].as_str()).collect();
    for want in ["node", "selection", "universe", "weighting", "document"] {
        assert!(
            groups.contains(want),
            "missing group {want}; got {groups:?}"
        );
    }

    // A record carries the full contract shape.
    let first = &tags[0];
    for key in [
        "name",
        "group",
        "kind",
        "forms",
        "output",
        "projections",
        "category",
        "doc",
        "since",
    ] {
        assert!(first.get(key).is_some(), "record missing key {key}");
    }
    // Every form carries its own shape/fields/payload — the v5 move off the tag.
    let form = &first["forms"][0];
    for key in ["shape", "fields", "payload"] {
        assert!(form.get(key).is_some(), "form missing key {key}");
    }

    let by_name = |n: &str| {
        tags.iter()
            .find(|t| t["name"] == n)
            .unwrap_or_else(|| panic!("!{n} is a tag"))
    };
    let field = |tag: &serde_json::Value, form: usize, name: &str| {
        tag["forms"][form]["fields"]
            .as_array()
            .expect("fields")
            .iter()
            .find(|f| f["name"] == name)
            .unwrap_or_else(|| panic!("no field {name}"))
            .clone()
    };

    // An expression slot says what it must be filled with, so a consumer can
    // offer only the tags whose `output` matches.
    let lhs = field(by_name("and"), 0, "lhs");
    assert_eq!(lhs["node_output"], serde_json::json!(["bool"]), "!and lhs");
    // Omitted, not null, on a slot that holds no free expression.
    let period = field(by_name("sma"), 0, "period");
    assert!(
        period.get("node_output").is_none(),
        "scalar field unstamped"
    );

    // The v5 point: a tag with more than one spelling reports all of them,
    // canonical first, each alternate carrying prose for why it exists.
    let changed = by_name("changed");
    let shapes: Vec<&str> = changed["forms"]
        .as_array()
        .expect("forms")
        .iter()
        .map(|f| f["shape"].as_str().expect("shape"))
        .collect();
    assert_eq!(shapes, ["newtype", "map"], "!changed is written two ways");
    assert!(
        changed["forms"][1]["doc"].is_string(),
        "an alternate spelling explains itself"
    );
    // The alternate's slot is stamped with the same demand as the canonical
    // payload — a consumer completing inside `!changed { source: ` needs it.
    assert_eq!(
        field(changed, 1, "source")["node_output"],
        changed["forms"][0]["payload_output"],
        "!changed's two spellings hold the same slot"
    );

    // `scope` is how a consumer learns that `group == "document"` is not a
    // position claim: `!param` goes anywhere, `!slot` only inside a template.
    assert!(
        by_name("param")["forms"][0].get("scope").is_none(),
        "!param is position-free"
    );
    assert_eq!(by_name("slot")["forms"][0]["scope"], "template");
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
    // `anyOf`, not `oneOf`: `root:` is optional on the single-asset shape, so a
    // document that omits it is structurally both a `single` and a `multi`.
    let any_of = schema["anyOf"]
        .as_array()
        .expect("document root is an anyOf");
    assert_eq!(any_of.len(), 5, "single/pairs/basket/multi/portfolio");
}
