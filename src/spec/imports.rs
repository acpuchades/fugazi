//! `!import` — splice another YAML document into a spec as a value.
//!
//! A strategy spec is a value tree, and a `!import` node is a *hole* in it: the
//! referenced document is parsed and its whole value takes the node's place. So
//! a shared exit rule, a sizing recipe, or a whole `long:` side can live in its
//! own file and be reused across strategies:
//!
//! ```yaml
//! # strategy.yml
//! symbol: BTC
//! long:
//!   enter: !import signals/breakout.yml
//!   exit: !crosses_below { lhs: close, rhs: !sma { period: 20 } }
//! sizing: !import sizing/half-kelly.yml
//! ```
//!
//! ## Inline params
//!
//! The object form
//! `!import { path: signals/breakout.yml, params: { FAST: 5, SLOW: 20 } }`
//! resolves the imported subtree's `!param` placeholders **against the
//! inline `params:` first**, and any placeholder whose key isn't listed
//! there falls through to the outer document's regular `--params` pass.
//! This is the natural shape for a portfolio-of-strategies spec, where the
//! same shared fragment is imported N times with N distinct
//! parameterizations:
//!
//! ```yaml
//! # portfolio.yml
//! children:
//!   - name: fast_trend
//!     strategy: !import { path: strategies/trend.yml, params: { FAST: 5,  SLOW: 20 } }
//!   - name: slow_trend
//!     strategy: !import { path: strategies/trend.yml, params: { FAST: 20, SLOW: 50 } }
//! ```
//!
//! Inline params are themselves a value tree — a value may be a scalar,
//! a fully-built subtree, or even another `!import` / `!param` node.
//! Nested imports inside an inline value resolve against the *outer*
//! document's directory (they belong to the importing document, not to
//! the file being imported), and any `!param` that bubbles up unresolved
//! is left for the outer pass.
//!
//! ## Passes and semantics
//!
//! Substitution runs on the **untyped value tree**, exactly like
//! [`crate::spec::params`] — the typed spec has no room for a placeholder where a
//! a boolean-output `NodeSpec` is expected, so the hole must be filled before typed parsing.
//! The pass order is `parse → imports → !param → typed parse`, which means an
//! imported document is itself a first-class spec fragment: it may contain its
//! own `!import`s (resolved relative to *its* directory) and its own `!param`
//! placeholders (resolved from the same `--params` table as the importing
//! document, so one table parameterises the whole tree).
//!
//! **Relative paths resolve against the importing document's directory**, not
//! the process's working directory — a strategy in `strategies/` importing
//! `shared/exit.yml` finds `strategies/shared/exit.yml` no matter where
//! `fugazi` was invoked from. Inline strategy text (no `@file`) has no
//! directory of its own, so its imports resolve against the working directory.
//!
//! **Every import is confined to a `root` directory**, independent of `base` —
//! [`resolve`] takes both. Confinement historically doubled as `base`
//! (`resolve(value, base)`, `root == base`), which is still what every CLI
//! call site and `--params`-style caller passes by default: the top-level
//! document's own directory. But a document two levels deep that wants a
//! fragment living in a *sibling* directory (`A/strategies/foo.yml` importing
//! `../fragments/bar.yml`, where the caller considers `A` the project root)
//! needs `root` wider than any one file's own `base` — hence the separate
//! parameter, e.g. the CLI's `--import-root`. `root` is expected to be `base`
//! or an ancestor of it; when it isn't, an import relative to the entry
//! document's own directory can fail confinement too, which reads as an
//! ordinary "outside the import root" error rather than a silent misconfiguration.
//! An absolute path, or a `..` that walks past `root`, is refused rather than
//! followed: `!import /etc/hostname` and `!import ../../../../etc/passwd` are
//! both hard errors, not filesystem reads. This matters for an embedder
//! driving [`crate::spec::load_value`] (or the Python `load_spec`/`optimize`
//! bindings) against user-authored documents, where `root` is the only thing
//! standing between an author's `enter:` field and the host's filesystem —
//! see [`refuse`] for a caller that wants no filesystem access at all.
//!
//! Import cycles are a hard error naming the chain, rather than a stack
//! overflow. So is a composed document that nests too deeply — see
//! [`MAX_DEPTH`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Map, Value};

/// The singleton key a `!import` tag normalizes to (see [`crate::spec::convert`]).
const IMPORT: &str = "import";

/// Whether `map` has the single-key shape a `!import` node normalizes to —
/// shared by [`import_directive`] (which also validates the body) and
/// [`refuse`] (which only needs to know whether to bail).
fn looks_like_import(map: &Map<String, Value>) -> bool {
    map.len() == 1 && map.contains_key(IMPORT)
}

/// One resolved `!import` directive: the path to load and the inline
/// `params:` (if any) to apply to the loaded document's own `!param`
/// placeholders.
struct ImportDirective {
    path: String,
    /// Inline params — keys the imported subtree's `!param` placeholders
    /// resolve against, before falling through to the outer pass.
    /// `None` for the bare-string form (no inline params).
    inline_params: Option<Map<String, Value>>,
}

/// Resolve every `!import` node in `value`, splicing in the document each one
/// names. `base` is the directory the *top-level* document's own relative
/// import paths resolve against — its own directory (see
/// [`crate::spec::input::Source::base_dir`]). `root` is the confinement
/// boundary: no import, however deeply nested, may resolve to a path outside
/// it (see the module docs). Pass `base` for both to get the historical
/// behavior of confining a document to its own directory.
pub fn resolve(value: Value, base: &Path, root: &Path) -> Result<Value> {
    walk(value, base, root, &mut Vec::new(), 0)
}

/// How deep the **composed** document may nest before resolution refuses it.
///
/// The YAML parser bounds a *single file* (serde's own recursion limit, 128
/// levels), and that bound is what keeps every later pass safe — `!param`
/// substitution, the typed `NodeSpec` parse and `try_build` are each a separate
/// recursion over the same tree. Splicing defeats it: sixty files of a hundred
/// levels each compose into six thousand, and the first of those four passes
/// overflowed the stack and aborted the process. A cycle was already an error
/// naming the chain; this is the same failure reached by a different route.
///
/// `256` is twice the parser's own per-file bound and an order of magnitude past
/// anything an author writes — a strategy document is ten or twenty levels deep,
/// and an import chain of five files is a large one. It is deliberately sized
/// against the *smallest* stack this code runs on rather than the main thread's:
/// a debug-build `walk` frame is around two kilobytes, and a spawned thread
/// (a `cargo test` worker, a Python thread) gets 2 MiB, so a limit chosen for
/// the main thread's 8 MiB would still abort there.
pub const MAX_DEPTH: usize = 256;

/// Structural check for a caller that disables `!import` entirely: walks
/// `value` exactly like [`resolve`] would, but bails on the first `!import`
/// node instead of loading it. No filesystem access — unlike `resolve`
/// confined to a root that happens to be empty or unreadable, this never
/// touches `std::fs` at all, so it's the right choice for a caller that wants
/// zero coupling between a document and the host filesystem rather than a
/// scoped one.
pub fn refuse(value: &Value) -> Result<()> {
    match value {
        Value::Object(map) => {
            if looks_like_import(map) {
                bail!(
                    "!import is disabled for this caller (no `base_dir`/filesystem \
                     access was granted)"
                );
            }
            map.values().try_for_each(refuse)
        }
        Value::Array(items) => items.iter().try_for_each(refuse),
        _ => Ok(()),
    }
}

/// Recurse the tree, replacing each `!import` node with the imported document.
/// `base` is the directory the *next* relative import resolves against (the
/// current file's own directory); `root` is the fixed confinement boundary —
/// the original top-level `base` [`resolve`] was called with, unchanged
/// across nested imports. `stack` carries the canonical paths of the
/// documents currently being resolved — the cycle tripwire.
fn walk(
    value: Value,
    base: &Path,
    root: &Path,
    stack: &mut Vec<PathBuf>,
    depth: usize,
) -> Result<Value> {
    if depth > MAX_DEPTH {
        bail!(
            "!import: the composed document nests more than {MAX_DEPTH} levels \
             deep. Each file is bounded on its own by the YAML parser, but splicing \
             them together is not — and the passes that follow (`!param` \
             substitution, the typed parse, the build) each recurse over the whole \
             tree. Flatten the import chain."
        );
    }
    match value {
        Value::Object(map) => {
            if let Some(directive) = import_directive(&map)? {
                return load(&directive, base, root, stack, depth);
            }
            let mut out = Map::with_capacity(map.len());
            for (key, v) in map {
                out.insert(key, walk(v, base, root, stack, depth + 1)?);
            }
            Ok(Value::Object(out))
        }
        Value::Array(items) => items
            .into_iter()
            .map(|v| walk(v, base, root, stack, depth + 1))
            .collect::<Result<Vec<_>>>()
            .map(Value::Array),
        scalar => Ok(scalar),
    }
}

/// Parse a `!import` node. Returns `None` when `map` isn't one; otherwise
/// returns the path to import (and, in the object form, any inline
/// `params:` to apply to the imported subtree).
///
/// Accepts two body shapes:
///
/// * A **string path** — the historical form `!import signals/breakout.yml`.
/// * An **object** with a required `path` key and optional `params` mapping
///   — `!import { path: signals/breakout.yml, params: { FAST: 5 } }`.
///
/// Anything else — a bare mapping without `path`, a non-string `path`, or
/// a scalar body that isn't a string — is a hard error, because leaving
/// it in place would be mistaken for a spec fragment and fail much later
/// with a confusing type error.
fn import_directive(map: &Map<String, Value>) -> Result<Option<ImportDirective>> {
    if !looks_like_import(map) {
        return Ok(None);
    }
    let body = &map[IMPORT];
    match body {
        Value::String(path) => Ok(Some(ImportDirective {
            path: path.clone(),
            inline_params: None,
        })),
        Value::Object(fields) => {
            let path = fields
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    anyhow!(
                        "!import object form needs a string `path` field \
                         (`!import {{ path: signals/breakout.yml, params: {{ … }} }}`)"
                    )
                })?
                .to_string();
            let inline_params = match fields.get("params") {
                None => None,
                Some(Value::Object(p)) => Some(p.clone()),
                Some(other) => {
                    bail!("!import `params` must be a mapping of NAME: value, got {other}")
                }
            };
            // Reject stray keys — the object form only recognises `path`
            // and `params`, and a typo (e.g. `parmas:`) would otherwise
            // silently drop on the floor.
            for key in fields.keys() {
                if key != "path" && key != "params" {
                    bail!(
                        "!import object form only recognises `path` and `params`, got unknown key `{key}`"
                    );
                }
            }
            Ok(Some(ImportDirective {
                path,
                inline_params,
            }))
        }
        other => bail!(
            "!import takes a file path (`!import signals/breakout.yml`) or an object \
             (`!import {{ path: …, params: {{ … }} }}`), got {other}"
        ),
    }
}

/// Load one imported document: read it relative to `base`, parse it (its own
/// `!tag`s normalize exactly like the importing document's), resolve its
/// nested imports against *its own* directory, and — if the directive
/// carried inline `params:` — apply those against the loaded tree via
/// [`crate::spec::params::substitute_partial`] before returning.
fn load(
    directive: &ImportDirective,
    base: &Path,
    root: &Path,
    stack: &mut Vec<PathBuf>,
    depth: usize,
) -> Result<Value> {
    let joined = base.join(&directive.path);
    let canonical = std::fs::canonicalize(&joined)
        .with_context(|| format!("!import {}: reading `{}`", directive.path, joined.display()))?;

    // Confine to `root` regardless of how the escape was spelled — an
    // absolute `directive.path` (which makes `join` discard `base` entirely),
    // a `..` that walks past it, or a symlink that resolves outside it are
    // all caught here, because `canonicalize` has already resolved every
    // symlink and `..` component on both sides.
    let canonical_root = std::fs::canonicalize(root).with_context(|| {
        format!(
            "!import {}: resolving import root `{}`",
            directive.path,
            root.display()
        )
    })?;
    if !canonical.starts_with(&canonical_root) {
        bail!(
            "!import {}: `{}` is outside the import root `{}`",
            directive.path,
            canonical.display(),
            canonical_root.display()
        );
    }

    if let Some(start) = stack.iter().position(|seen| *seen == canonical) {
        let chain: Vec<String> = stack[start..]
            .iter()
            .chain(std::iter::once(&canonical))
            .map(|p| p.display().to_string())
            .collect();
        bail!("!import cycle: {}", chain.join(" -> "));
    }

    let text = std::fs::read_to_string(&canonical).with_context(|| {
        format!(
            "!import {}: reading `{}`",
            directive.path,
            canonical.display()
        )
    })?;
    let value = crate::spec::input::parse_value_at(&text, &canonical.display().to_string())
        .with_context(|| {
            format!(
                "!import {}: parsing `{}`",
                directive.path,
                canonical.display()
            )
        })?;

    let dir = canonical
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    stack.push(canonical);
    // The imported document continues the *composed* depth, not a fresh one —
    // that is exactly the accounting splicing was defeating.
    let resolved = walk(value, &dir, root, stack, depth);
    stack.pop();
    let resolved = resolved?;

    // No inline params: fast path, return the resolved tree as-is.
    let Some(inline) = directive.inline_params.as_ref() else {
        return Ok(resolved);
    };

    // Inline values are themselves untyped subtrees — they may contain
    // nested `!import` nodes (resolved against the *outer* document's
    // directory, not the imported one — they belong to the importing
    // document) or `!param` placeholders (left as-is for the outer pass).
    let mut inline_resolved: HashMap<String, Value> = HashMap::with_capacity(inline.len());
    for (key, value) in inline {
        inline_resolved.insert(key.clone(), walk(value.clone(), base, root, stack, depth)?);
    }
    crate::spec::params::substitute_partial(resolved, &inline_resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A fresh directory under the system temp dir, so each test's imported
    /// files (and its relative paths) are independent.
    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fugazi_imports_{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(dir: &Path, name: &str, text: &str) {
        let path = dir.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn resolve_text(text: &str, base: &Path) -> Result<Value> {
        resolve(crate::spec::input::parse_value(text).unwrap(), base, base)
    }

    #[test]
    fn splices_an_imported_document_in_as_a_value() {
        let dir = tmp_dir("splice");
        write(
            &dir,
            "enter.yml",
            "!crosses_above { lhs: !sma { period: 3 }, rhs: !sma { period: 8 } }",
        );

        let value = resolve_text("root: BTC\nlong:\n  enter: !import enter.yml\n", &dir).unwrap();

        let expected = crate::spec::input::parse_value(
            "root: BTC\nlong:\n  enter: !crosses_above { lhs: !sma { period: 3 }, rhs: !sma { period: 8 } }\n",
        )
        .unwrap();
        assert_eq!(value, expected);
    }

    #[test]
    fn a_nested_import_resolves_against_the_importing_files_directory() {
        // `strategy.yml` imports `parts/side.yml`, which imports `enter.yml`
        // *next to itself* — so the inner path is `parts/enter.yml`, not
        // `enter.yml` relative to the top-level document.
        let dir = tmp_dir("nested");
        write(&dir, "parts/side.yml", "enter: !import enter.yml\n");
        write(&dir, "parts/enter.yml", "!value true\n");

        let value = resolve_text("long: !import parts/side.yml\n", &dir).unwrap();
        let expected = crate::spec::input::parse_value("long:\n  enter: !value true\n").unwrap();
        assert_eq!(value, expected);
    }

    #[test]
    fn a_cycle_is_an_error_naming_the_chain() {
        let dir = tmp_dir("cycle");
        write(&dir, "a.yml", "enter: !import b.yml\n");
        write(&dir, "b.yml", "exit: !import a.yml\n");

        let err = resolve_text("long: !import a.yml\n", &dir)
            .unwrap_err()
            .to_string();
        assert!(err.contains("!import cycle"), "{err}");
        assert!(err.contains("a.yml"), "{err}");
        assert!(err.contains("b.yml"), "{err}");
    }

    #[test]
    fn a_missing_file_errors_with_the_path_it_looked_for() {
        let dir = tmp_dir("missing");
        let err = resolve_text("enter: !import nope.yml\n", &dir)
            .unwrap_err()
            .to_string();
        assert!(err.contains("nope.yml"), "{err}");
    }

    #[test]
    fn object_body_without_path_is_rejected() {
        let dir = tmp_dir("body");
        let err = resolve_text("enter: !import { period: 3 }\n", &dir)
            .unwrap_err()
            .to_string();
        assert!(err.contains("`path` field"), "{err}");
    }

    #[test]
    fn object_body_with_unknown_key_is_rejected() {
        // A typo (`parmas:` instead of `params:`) would silently drop
        // on the floor without this guard.
        let dir = tmp_dir("unknown_key");
        write(&dir, "x.yml", "!value 1\n");
        let err = resolve_text(
            "enter: !import { path: x.yml, parmas: { FAST: 3 } }\n",
            &dir,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("unknown key `parmas`"), "{err}");
    }

    #[test]
    fn a_non_string_scalar_body_is_rejected() {
        let dir = tmp_dir("scalar");
        let err = resolve_text("enter: !import 3\n", &dir)
            .unwrap_err()
            .to_string();
        assert!(err.contains("!import takes a file path"), "{err}");
    }

    #[test]
    fn inline_params_resolve_placeholders_in_the_imported_subtree() {
        // The classic ask: one shared strategy fragment, imported twice
        // with different `params:` — each import produces the right
        // FAST/SLOW pair without a --params call from the caller.
        let dir = tmp_dir("inline_params");
        write(
            &dir,
            "trend.yml",
            "period_fast: !param FAST\nperiod_slow: !param SLOW\n",
        );

        let value = resolve_text(
            "\
             a: !import { path: trend.yml, params: { FAST: 5,  SLOW: 20 } }\n\
             b: !import { path: trend.yml, params: { FAST: 20, SLOW: 50 } }\n\
             ",
            &dir,
        )
        .unwrap();
        let expected = crate::spec::input::parse_value(
            "\
             a: { period_fast: 5,  period_slow: 20 }\n\
             b: { period_fast: 20, period_slow: 50 }\n\
             ",
        )
        .unwrap();
        assert_eq!(value, expected);
    }

    #[test]
    fn placeholders_not_covered_by_inline_params_fall_through() {
        // FAST is in the inline table so it resolves at import time.
        // SLOW isn't — it survives the imports pass and the outer
        // params::substitute pass resolves it against --params.
        let dir = tmp_dir("fall_through");
        write(&dir, "trend.yml", "fast: !param FAST\nslow: !param SLOW\n");

        let value = resolve_text(
            "cfg: !import { path: trend.yml, params: { FAST: 5 } }\n",
            &dir,
        )
        .unwrap();
        // Only FAST resolved; SLOW is still a placeholder.
        let params = std::collections::HashMap::from([("SLOW".to_string(), Value::from(50))]);
        let value = crate::spec::params::substitute(value, &params).unwrap();
        let expected = crate::spec::input::parse_value("cfg: { fast: 5, slow: 50 }\n").unwrap();
        assert_eq!(value, expected);
    }

    #[test]
    fn inline_params_do_not_eagerly_apply_defaults() {
        // A placeholder with a `default:` and no inline coverage must
        // survive the partial pass — otherwise the outer --params
        // couldn't override its default.
        let dir = tmp_dir("defaults");
        write(&dir, "x.yml", "value: !param { key: FAST, default: 99 }\n");

        // No inline table: default kicks in on the outer pass, --params
        // wins over it.
        let value = resolve_text("cfg: !import { path: x.yml, params: {} }\n", &dir).unwrap();
        let params = std::collections::HashMap::from([("FAST".to_string(), Value::from(3))]);
        let value = crate::spec::params::substitute(value, &params).unwrap();
        let expected = crate::spec::input::parse_value("cfg: { value: 3 }\n").unwrap();
        assert_eq!(value, expected);

        // Same import, no --params: default applies on the outer pass.
        let value = resolve_text("cfg: !import { path: x.yml, params: {} }\n", &dir).unwrap();
        let value =
            crate::spec::params::substitute(value, &std::collections::HashMap::new()).unwrap();
        let expected = crate::spec::input::parse_value("cfg: { value: 99 }\n").unwrap();
        assert_eq!(value, expected);
    }

    #[test]
    fn nested_import_inherits_outer_inline_params() {
        // A imports B; the outer import of A provides `FAST: 5` inline.
        // The FAST placeholder in B (spliced into A during A's own
        // walk) should resolve on A's partial pass — inline params see
        // the whole resolved-A subtree.
        let dir = tmp_dir("nested_inline");
        write(&dir, "a.yml", "inner: !import b.yml\n");
        write(&dir, "b.yml", "fast: !param FAST\n");

        let value =
            resolve_text("cfg: !import { path: a.yml, params: { FAST: 5 } }\n", &dir).unwrap();
        let expected = crate::spec::input::parse_value("cfg: { inner: { fast: 5 } }\n").unwrap();
        assert_eq!(value, expected);
    }

    #[test]
    fn inner_inline_wins_over_outer_inline_for_the_inner_subtree() {
        // A imports B with `FAST: 99` inline; the outer import of A
        // says `FAST: 5`. B's placeholder resolves at B's partial pass
        // (against 99), so A's later partial pass sees no FAST to
        // resolve.
        let dir = tmp_dir("inner_wins");
        write(
            &dir,
            "a.yml",
            "inner: !import { path: b.yml, params: { FAST: 99 } }\n",
        );
        write(&dir, "b.yml", "fast: !param FAST\n");

        let value =
            resolve_text("cfg: !import { path: a.yml, params: { FAST: 5 } }\n", &dir).unwrap();
        let expected = crate::spec::input::parse_value("cfg: { inner: { fast: 99 } }\n").unwrap();
        assert_eq!(value, expected);
    }

    #[test]
    fn inline_param_value_may_itself_reference_a_placeholder() {
        // Inline params values are ordinary subtrees — passing a
        // `!param OUTER` as the value threads the outer `--params`
        // table through the import boundary. Useful when a portfolio
        // spec picks up a top-level parameter and forwards it into
        // several children.
        let dir = tmp_dir("inline_ref");
        write(&dir, "x.yml", "fast: !param FAST\n");

        let value = resolve_text(
            "cfg: !import { path: x.yml, params: { FAST: !param OUTER } }\n",
            &dir,
        )
        .unwrap();
        // The inline partial pass replaces `!param FAST` (inside x.yml)
        // with the placeholder `!param OUTER` from the outer document;
        // the outer pass then resolves it against --params.
        let params = std::collections::HashMap::from([("OUTER".to_string(), Value::from(7))]);
        let value = crate::spec::params::substitute(value, &params).unwrap();
        let expected = crate::spec::input::parse_value("cfg: { fast: 7 }\n").unwrap();
        assert_eq!(value, expected);
    }

    #[test]
    fn inline_param_value_may_itself_be_an_import() {
        // Inline params values can be `!import` nodes — the imports
        // pass resolves those (against the outer document's dir)
        // before the partial substitute splices them into the imported
        // subtree.
        let dir = tmp_dir("inline_import");
        write(&dir, "outer.yml", "score: !param SCORE\n");
        write(&dir, "score.yml", "!value 42\n");

        let value = resolve_text(
            "cfg: !import { path: outer.yml, params: { SCORE: !import score.yml } }\n",
            &dir,
        )
        .unwrap();
        // score.yml resolves to `!value 42`, which is spliced in as
        // the SCORE value inside outer.yml.
        let expected = crate::spec::input::parse_value("cfg: { score: !value 42 }\n").unwrap();
        assert_eq!(value, expected);
    }

    #[test]
    fn a_document_without_imports_is_returned_verbatim() {
        let dir = tmp_dir("noop");
        let text = "symbol: BTC\nlong:\n  enter: !gt { lhs: close, rhs: !value 10 }\n";
        assert_eq!(
            resolve_text(text, &dir).unwrap(),
            crate::spec::input::parse_value(text).unwrap(),
        );
    }

    #[test]
    fn an_absolute_path_is_refused_even_when_the_file_exists() {
        // Regression: `PathBuf::join` discards `base` entirely when the
        // joinee is absolute, so `!import /etc/hostname` used to read
        // straight off the host filesystem instead of erroring.
        let dir = tmp_dir("absolute");
        let outside = tmp_dir("absolute_target");
        write(&outside, "secret.yml", "!value 1\n");
        let absolute = outside.join("secret.yml");

        let err = resolve_text(&format!("enter: !import {}\n", absolute.display()), &dir)
            .unwrap_err()
            .to_string();
        assert!(err.contains("outside the import root"), "{err}");
    }

    #[test]
    fn a_relative_escape_via_dotdot_is_refused() {
        let dir = tmp_dir("dotdot_root");
        let sub = dir.join("sub");
        fs::create_dir_all(&sub).unwrap();
        let outside = tmp_dir("dotdot_outside");
        write(&outside, "secret.yml", "!value 1\n");
        let escape = format!(
            "../../{}/secret.yml",
            outside.file_name().unwrap().to_str().unwrap()
        );

        let err = resolve_text(&format!("enter: !import {escape}\n"), &sub)
            .unwrap_err()
            .to_string();
        assert!(err.contains("outside the import root"), "{err}");
    }

    #[test]
    fn a_nested_import_may_not_escape_the_top_level_root_either() {
        // The per-file directory a nested import resolves relative to keeps
        // moving (see `a_nested_import_resolves_against_the_importing_files_directory`),
        // but the confinement root stays pinned to the *original* `base` —
        // a file two levels deep can't walk back out past it.
        let dir = tmp_dir("nested_escape_root");
        let outside = tmp_dir("nested_escape_outside");
        write(&outside, "secret.yml", "!value 1\n");
        // `parts/side.yml` sits two levels under `/tmp`, so `../..` from
        // there reaches `/tmp` — outside `dir`, the confinement root.
        let escape = format!(
            "../../{}/secret.yml",
            outside.file_name().unwrap().to_str().unwrap()
        );
        write(
            &dir,
            "parts/side.yml",
            &format!("enter: !import {escape}\n"),
        );

        let err = resolve_text("long: !import parts/side.yml\n", &dir)
            .unwrap_err()
            .to_string();
        assert!(err.contains("outside the import root"), "{err}");
    }

    #[test]
    fn a_relative_import_that_stays_within_root_still_works() {
        // Confinement shouldn't break the ordinary case: a subdirectory
        // import, reached via a `..` that never actually leaves `base`.
        let dir = tmp_dir("within_root");
        write(&dir, "shared/exit.yml", "!value 1\n");
        write(
            &dir,
            "strategies/enter.yml",
            "enter: !import ../shared/exit.yml\n",
        );

        let value = resolve_text("long: !import strategies/enter.yml\n", &dir).unwrap();
        let expected = crate::spec::input::parse_value("long:\n  enter: !value 1\n").unwrap();
        assert_eq!(value, expected);
    }

    #[test]
    fn an_explicit_root_wider_than_base_reaches_a_sibling_directory() {
        // `A/strategies/foo.yml` (base = `A/strategies`) wants a fragment at
        // `A/fragments/bar.yml` — a sibling, not a descendant of its own
        // directory. With `root == base` (the default every CLI call site
        // passes) that `..` walks outside root and is refused, exactly like
        // `a_relative_escape_via_dotdot_is_refused`. Passing a wider `root`
        // (here, `A` itself) is what lets it through while still refusing
        // anything outside `A`.
        let project = tmp_dir("wider_root_project");
        let strategies_dir = project.join("strategies");
        fs::create_dir_all(&strategies_dir).unwrap();
        write(&project, "fragments/bar.yml", "!value 42\n");

        let value = resolve(
            crate::spec::input::parse_value("cfg: !import ../fragments/bar.yml\n").unwrap(),
            &strategies_dir,
            &project,
        )
        .unwrap();
        let expected = crate::spec::input::parse_value("cfg: !value 42\n").unwrap();
        assert_eq!(value, expected);

        // Confinement still holds against the wider root: a `..` that walks
        // past `project` itself is refused just like before.
        let outside = tmp_dir("wider_root_outside");
        write(&outside, "secret.yml", "!value 1\n");
        let escape = format!(
            "../../{}/secret.yml",
            outside.file_name().unwrap().to_str().unwrap()
        );
        let err = resolve(
            crate::spec::input::parse_value(&format!("cfg: !import {escape}\n")).unwrap(),
            &strategies_dir,
            &project,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("outside the import root"), "{err}");
    }

    #[test]
    fn refuse_bails_on_a_bare_string_import() {
        // No filesystem involved at all — `enter.yml` need not even exist for
        // this to be caught, unlike `resolve`.
        let value = crate::spec::input::parse_value("enter: !import enter.yml\n").unwrap();
        let err = refuse(&value).unwrap_err().to_string();
        assert!(err.contains("disabled"), "{err}");
    }

    #[test]
    fn refuse_bails_on_an_import_nested_inside_a_template_body() {
        let value =
            crate::spec::input::parse_value("score: !mul { lhs: !import a.yml, rhs: !value 2 }\n")
                .unwrap();
        let err = refuse(&value).unwrap_err().to_string();
        assert!(err.contains("disabled"), "{err}");
    }

    #[test]
    fn refuse_accepts_a_document_with_no_import_at_all() {
        let value = crate::spec::input::parse_value(
            "symbol: BTC\nlong:\n  enter: !gt { lhs: close, rhs: !value 10 }\n",
        )
        .unwrap();
        assert!(refuse(&value).is_ok());
    }

    /// Splicing defeats the parser's own recursion bound.
    ///
    /// serde limits a *single file* to 128 levels, and that is what keeps the
    /// three passes after this one safe — `!param` substitution, the typed
    /// `NodeSpec` parse and `try_build` each recurse over the whole tree.
    /// Composition is not bounded by it: sixty files of a hundred levels
    /// compose into six thousand, and `walk` — the first of those passes —
    /// overflowed the stack and aborted the process rather than reporting
    /// anything. A cycle was already an error naming the chain; this is the
    /// same failure by a different route, so it is an error too.
    #[test]
    fn a_composed_document_deeper_than_the_limit_is_an_error_not_a_stack_overflow() {
        let dir = tmp_dir("deep_chain");
        // Each file wraps the previous one in `WRAP` `!abs` levels — two object
        // levels apiece, since `!abs { source: … }` normalizes to
        // `{abs: {source: …}}` — so the chain composes to `files * WRAP * 2`,
        // comfortably past `MAX_DEPTH` while every individual file stays well
        // inside the parser's own 128-level bound.
        const WRAP: usize = 50;
        let files = MAX_DEPTH / WRAP + 2;
        fs::write(dir.join("f0.yml"), "!close\n").unwrap();
        for i in 1..=files {
            let mut body = format!("!import f{}.yml", i - 1);
            for _ in 0..WRAP {
                body = format!("!abs {{ source: {body} }}");
            }
            fs::write(dir.join(format!("f{i}.yml")), body + "\n").unwrap();
        }

        let root = serde_json::json!({ "import": format!("f{files}.yml") });
        let err = resolve(root, &dir, &dir).expect_err("a chain this deep must be refused");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("nests more than"),
            "the error should name the depth bound, got: {msg}"
        );

        // A chain that stays inside the bound still composes — the guard is a
        // ceiling, not a ban on nested imports.
        let shallow = serde_json::json!({ "import": "f2.yml" });
        assert!(
            resolve(shallow, &dir, &dir).is_ok(),
            "two files of {WRAP} wraps is {} levels, inside the bound",
            2 * WRAP * 2
        );
    }
}
