//! Guards for the repo's **hand-maintained mirrors** — the places where one
//! list has to repeat another with no compile-time link between them.
//!
//! `NodeSpec`'s deserialization twin used to be this file's biggest customer:
//! `NodeSpecRaw` and its `From` impl were ~1,650 hand-maintained lines,
//! guarded here by regex-over-source comparisons of the two enum bodies. Both
//! are now **generated** by `#[derive(SpecRaw)]` (`fugazi-derive/src/raw.rs`)
//! from `NodeSpec`'s own definition, so that mirror — and its guards — no
//! longer exist: a variant or serde attribute is stated once and cannot be
//! forgotten on the twin.

use std::collections::BTreeSet;

use fugazi::spec::grammar::spec_grammar;

// ---------------------------------------------------------------------------
// `src/metrics.rs` / `python/src/metrics.rs`
// ---------------------------------------------------------------------------

/// Every `pub fn` in `src/metrics.rs` must be registered on `fugazi.metrics`.
///
/// `docs/CONTRIBUTING.md`'s "add a metric" step 5 is "bind it: `#[pyfunction]`
/// plus the name in `register_metrics_module`'s `reg!(...)`", and nothing
/// checked it. `python/tests/test_parity.py` covers the *tag* vocabulary and
/// the wallet surface, but has no reference to metrics at all — so a new metric
/// could ship Rust-only and the omission would surface as a user's
/// `AttributeError`.
///
/// All 57 are bound today; this keeps it that way. Deliberately Rust-side: it
/// runs in `cargo test`, so a contributor who never builds the wheel still
/// sees it.
#[test]
fn every_rust_metric_is_bound_on_the_python_module() {
    const RUST: &str = include_str!("../src/metrics.rs");
    const BINDINGS: &str = include_str!("../python/src/metrics.rs");

    let exported: BTreeSet<&str> = RUST
        .lines()
        .filter_map(|l| l.strip_prefix("pub fn "))
        .map(|rest| {
            rest.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .next()
                .unwrap_or_default()
        })
        .filter(|n| !n.is_empty())
        .collect();

    // The `reg!(a, b, c, …)` call inside `register_metrics_module`.
    let reg = BINDINGS
        .find("reg!(")
        .expect("python/src/metrics.rs must call reg!(...)");
    let end = BINDINGS[reg..]
        .find(");")
        .expect("reg!(...) must be closed")
        + reg;
    let registered: BTreeSet<&str> = BINDINGS[reg + "reg!(".len()..end]
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|t| !t.is_empty())
        .collect();

    let missing: Vec<&&str> = exported.difference(&registered).collect();
    assert!(
        missing.is_empty(),
        "these `src/metrics.rs` functions are not in \
         `register_metrics_module`'s `reg!(...)`: {missing:?}",
    );
    assert!(
        exported.len() > 40,
        "sanity: found only {} exported metrics",
        exported.len()
    );
}

// ---------------------------------------------------------------------------
// The grammar field-type vocabulary / `python/tests/test_spec_json_schema.py`
// ---------------------------------------------------------------------------

/// Every field type the grammar descriptor emits must have a dummy value in
/// `python/tests/test_spec_json_schema.py`.
///
/// That test builds a minimal instance of every tag *from the descriptor*, so
/// it needs one sample value per field type. Adding a type to the vocabulary
/// therefore breaks it — and only under `pytest`, which a Rust-only change
/// never runs. Adding `positive_uint` did exactly that: `cargo test` was green,
/// `tests/spec_json_schema.rs`'s own `FIELD_TYPES` had been updated, and CI's
/// Python job failed on a `KeyError`.
///
/// Checked here, in `cargo test`, so the two lists can't drift again.
#[test]
fn every_grammar_field_type_has_a_python_dummy_value() {
    const PY: &str = include_str!("../python/tests/test_spec_json_schema.py");

    let body = {
        let start = PY
            .find("def _dummy(ty):")
            .expect("test_spec_json_schema.py must define _dummy");
        let end = PY[start..]
            .find("}[ty]")
            .expect("_dummy must end in `}[ty]`")
            + start;
        &PY[start..end]
    };
    let known: BTreeSet<&str> = body
        .lines()
        .filter_map(|l| l.trim().strip_prefix('"'))
        .filter_map(|r| r.split_once("\":"))
        .map(|(k, _)| k)
        .collect();

    // Mirror the Python test's own filter: it skips the document-level groups,
    // which aren't expression nodes, and only fills *required* fields.
    let grammar = spec_grammar();
    let mut needed: BTreeSet<&str> = BTreeSet::new();
    for tag in &grammar {
        if tag.group != "node" && tag.group != "selection" {
            continue;
        }
        for form in &tag.forms {
            needed.extend(
                form.fields
                    .iter()
                    .filter(|f| f.required)
                    .map(|f| f.ty.as_str()),
            );
            if let Some(p) = form.payload.as_deref() {
                needed.insert(p);
            }
        }
    }

    let missing: Vec<&&str> = needed.difference(&known).collect();
    assert!(
        missing.is_empty(),
        "python/tests/test_spec_json_schema.py::_dummy has no value for {missing:?} — \
         that test constructs an instance of every tag from the descriptor, so a new \
         field type needs a sample there or its `pytest` run fails with a KeyError",
    );
}

/// `docs/CLI.md`'s provider table repeats `KNOWN_PROVIDERS` with nothing linking
/// them, and `fugazi list sources` prints the array rather than the table — so
/// the two drift silently in both directions. A documented id that isn't in the
/// array is a command that fails with `unknown provider`; an array entry that
/// isn't documented is undiscoverable outside `--help`.
///
/// Textual on both sides: `KNOWN_PROVIDERS` is `pub(crate)` in the binary, so
/// an integration test cannot read the array itself.
#[test]
fn the_cli_doc_lists_exactly_the_providers_get_accepts() {
    const GET_RS: &str = include_str!("../src/cli/get.rs");
    const CLI_MD: &str = include_str!("../docs/CLI.md");

    let array = {
        let start = GET_RS
            .find("KNOWN_PROVIDERS: &[ProviderInfo] = &[")
            .expect("src/cli/get.rs must define KNOWN_PROVIDERS");
        let end = GET_RS[start..]
            .find("\n];")
            .expect("KNOWN_PROVIDERS must end in `];` at column 0")
            + start;
        &GET_RS[start..end]
    };
    let known: BTreeSet<&str> = array
        .lines()
        .filter_map(|l| l.trim().strip_prefix("name: \""))
        .filter_map(|rest| rest.split_once('"'))
        .map(|(name, _)| name)
        .collect();
    assert!(
        !known.is_empty(),
        "parsed no provider names out of KNOWN_PROVIDERS"
    );

    let documented: BTreeSet<&str> = {
        let start = CLI_MD
            .find("| Provider | Grammar | Description |")
            .expect("docs/CLI.md must carry the provider table");
        CLI_MD[start..]
            .lines()
            .skip(2) // header row, separator row
            .take_while(|l| l.starts_with("| `"))
            .filter_map(|l| l.trim_start_matches("| `").split_once('`'))
            .map(|(name, _)| name)
            .collect()
    };

    assert_eq!(
        known, documented,
        "docs/CLI.md's provider table and `KNOWN_PROVIDERS` in src/cli/get.rs have drifted",
    );
}
