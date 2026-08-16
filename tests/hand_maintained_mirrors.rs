//! Guards for the repo's **hand-maintained mirrors** — the places where one
//! list has to repeat another with no compile-time link between them.
//!
//! # `NodeSpec` / `NodeSpecRaw`
//!
//! `NodeSpec` carries `#[serde(try_from = "serde_norway::Value")]`, so it can't
//! derive its own body — `NodeSpecRaw` is the deserializable twin, and a
//! `From<NodeSpecRaw>` impl of ~138 identity arms bridges them. That mirror is
//! ~900 hand-maintained lines with no compile-time link to the enum it mirrors.
//!
//! Two existing mechanisms already cover most of it, and they are the reason
//! this file is narrow rather than a full reflection harness:
//!
//! * A **new variant** must be classified in `typecheck.rs`'s two exhaustive
//!   matches, so it cannot reach the mirror step without the compiler stopping
//!   you first.
//! * A dropped `#[serde(default = "…")]` on a **non-`Option`** field is a type
//!   error — none of `Box<NodeSpec>`, `NonZeroUsize` or `Real` implements
//!   `Default`, which is every named-default field in the enum today.
//!
//! What neither catches is a dropped **bare `#[serde(default)]` on an `Option`
//! field**. That compiles clean and silently makes the key required, so
//! `spec_grammar()` — which reflects off `NodeSpec` — keeps advertising the
//! documented default while the parser, which goes through the mirror, rejects
//! a document that omits it. `!close` with no `source:` stops parsing, and
//! nothing points at the mirror.
//!
//! These are textual comparisons of the two enum bodies. Coarser than
//! reflection, but the mirror is private and the drift is itself textual: an
//! attribute that didn't get copied.

use std::collections::{BTreeMap, BTreeSet};

const SOURCE: &str = include_str!("../src/spec/expr.rs");

/// The body of `enum <name> { … }`, from the opening brace to the matching
/// close at column 0.
fn enum_body(name: &str) -> &'static str {
    let head = SOURCE
        .find(&format!("enum {name} {{"))
        .unwrap_or_else(|| panic!("`enum {name}` not found in src/spec/expr.rs"));
    let open = SOURCE[head..].find('{').expect("enum has a body") + head;
    let close = SOURCE[open..]
        .find("\n}")
        .unwrap_or_else(|| panic!("`enum {name}` body is not closed at column 0"))
        + open;
    &SOURCE[open + 1..close]
}

/// Variant names in an enum body: an identifier at exactly one indent level,
/// starting a line, beginning with an uppercase letter.
fn variants(body: &str) -> BTreeSet<String> {
    body.lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("    ")?;
            if rest.starts_with(' ') || rest.starts_with('#') || rest.starts_with('/') {
                return None;
            }
            let ident: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            let first = ident.chars().next()?;
            first.is_ascii_uppercase().then_some(ident)
        })
        .collect()
}

/// `variant.field -> the serde default spelling` for every defaulted field.
///
/// `Some(fn_name)` for `#[serde(default = "fn_name")]`, `None` for a bare
/// `#[serde(default)]`. Walks the body statefully because a field's attribute
/// sits on the line above it and the variant header some lines above that.
fn serde_defaults(body: &str) -> BTreeMap<String, Option<String>> {
    let mut out = BTreeMap::new();
    let mut variant = String::new();
    let mut pending: Option<Option<String>> = None;

    for line in body.lines() {
        let trimmed = line.trim();

        // A variant header sits at one indent level.
        if let Some(rest) = line.strip_prefix("    ")
            && !rest.starts_with(' ')
            && !rest.starts_with('#')
            && !rest.starts_with('/')
            && rest.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        {
            variant = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            pending = None;
            continue;
        }

        if trimmed.starts_with("#[serde(") && trimmed.contains("default") {
            pending = Some(
                trimmed
                    .split_once("default = \"")
                    .map(|(_, r)| r.split('"').next().unwrap_or_default().to_string()),
            );
            continue;
        }

        // A field line: `name: Type,`. The attribute directly above it applies.
        if let Some((name, _)) = trimmed.split_once(FIELD_SEP) {
            let name = name.trim();
            if !variant.is_empty()
                && !name.is_empty()
                && name.chars().all(|c| c.is_ascii_lowercase() || c == '_')
                && let Some(d) = pending.take()
            {
                out.insert(format!("{variant}.{name}"), d);
            }
        }
    }
    out
}

/// The `field: Type` separator. Named rather than inlined so the pattern above
/// reads as one thing.
const FIELD_SEP: &str = ": ";

#[test]
fn the_mirror_has_every_variant() {
    let spec = variants(enum_body("NodeSpec"));
    let raw = variants(enum_body("NodeSpecRaw"));

    let missing: Vec<&String> = spec.difference(&raw).collect();
    let extra: Vec<&String> = raw.difference(&spec).collect();

    assert!(
        missing.is_empty(),
        "`NodeSpecRaw` is missing {missing:?}. Adding a variant to `NodeSpec` \
         alone compiles clean but never parses — the mirror is the \
         deserializable twin.",
    );
    assert!(
        extra.is_empty(),
        "`NodeSpecRaw` has {extra:?}, which `NodeSpec` doesn't. A removed \
         variant has to leave both.",
    );
    assert!(spec.len() > 100, "sanity: parsed only {} variants", spec.len());
}

#[test]
fn the_mirror_repeats_every_serde_default() {
    let spec = serde_defaults(enum_body("NodeSpec"));
    let raw = serde_defaults(enum_body("NodeSpecRaw"));

    let mut wrong: Vec<String> = Vec::new();
    for (field, want) in &spec {
        match raw.get(field) {
            None => wrong.push(format!(
                "{field}: `NodeSpec` defaults it, the mirror does not — the key \
                 becomes required at parse while `spec_grammar()` keeps \
                 advertising the documented default"
            )),
            Some(got) if got != want => wrong.push(format!(
                "{field}: `NodeSpec` says {want:?}, the mirror says {got:?}"
            )),
            Some(_) => {}
        }
    }

    assert!(wrong.is_empty(), "serde defaults drifted:\n  {}", wrong.join("\n  "));
    assert!(
        spec.len() > 50,
        "sanity: parsed only {} defaulted fields",
        spec.len()
    );
}

// ---------------------------------------------------------------------------
// `src/metrics.rs` / `python/src/metrics.rs`
// ---------------------------------------------------------------------------

/// Every `pub fn` in `src/metrics.rs` must be registered on `fugazi.metrics`.
///
/// `doc/CONTRIBUTING.md`'s "add a metric" step 5 is "bind it: `#[pyfunction]`
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
