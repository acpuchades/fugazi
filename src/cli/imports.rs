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
//! [`crate::params`] — the typed spec has no room for a placeholder where a
//! `SignalSpec` is expected, so the hole must be filled before typed parsing.
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
//! Import cycles are a hard error naming the chain, rather than a stack
//! overflow.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Map, Value};

/// The singleton key a `!import` tag normalizes to (see [`crate::convert`]).
const IMPORT: &str = "import";

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
/// names. `base` is the directory relative import paths resolve against — the
/// importing document's own directory (see [`crate::input::Source::base_dir`]).
pub fn resolve(value: Value, base: &Path) -> Result<Value> {
    walk(value, base, &mut Vec::new())
}

/// Recurse the tree, replacing each `!import` node with the imported document.
/// `stack` carries the canonical paths of the documents currently being
/// resolved — the cycle tripwire.
fn walk(value: Value, base: &Path, stack: &mut Vec<PathBuf>) -> Result<Value> {
    match value {
        Value::Object(map) => {
            if let Some(directive) = import_directive(&map)? {
                return load(&directive, base, stack);
            }
            let mut out = Map::with_capacity(map.len());
            for (key, v) in map {
                out.insert(key, walk(v, base, stack)?);
            }
            Ok(Value::Object(out))
        }
        Value::Array(items) => items
            .into_iter()
            .map(|v| walk(v, base, stack))
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
    if map.len() != 1 {
        return Ok(None);
    }
    let Some(body) = map.get(IMPORT) else {
        return Ok(None);
    };
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
                Some(other) => bail!(
                    "!import `params` must be a mapping of NAME: value, got {other}"
                ),
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
/// [`crate::params::substitute_partial`] before returning.
fn load(directive: &ImportDirective, base: &Path, stack: &mut Vec<PathBuf>) -> Result<Value> {
    let joined = base.join(&directive.path);
    let canonical = std::fs::canonicalize(&joined).with_context(|| {
        format!("!import {}: reading `{}`", directive.path, joined.display())
    })?;

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
    let value = crate::input::parse_value_at(&text, &canonical.display().to_string())
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
    let resolved = walk(value, &dir, stack);
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
        inline_resolved.insert(key.clone(), walk(value.clone(), base, stack)?);
    }
    crate::params::substitute_partial(resolved, &inline_resolved)
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
        resolve(crate::input::parse_value(text).unwrap(), base)
    }

    #[test]
    fn splices_an_imported_document_in_as_a_value() {
        let dir = tmp_dir("splice");
        write(
            &dir,
            "enter.yml",
            "!crosses_above { lhs: !sma { period: 3 }, rhs: !sma { period: 8 } }",
        );

        let value = resolve_text(
            "symbol: BTC\nlong:\n  enter: !import enter.yml\n",
            &dir,
        )
        .unwrap();

        let expected = crate::input::parse_value(
            "symbol: BTC\nlong:\n  enter: !crosses_above { lhs: !sma { period: 3 }, rhs: !sma { period: 8 } }\n",
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
        let expected = crate::input::parse_value("long:\n  enter: !value true\n").unwrap();
        assert_eq!(value, expected);
    }

    #[test]
    fn a_cycle_is_an_error_naming_the_chain() {
        let dir = tmp_dir("cycle");
        write(&dir, "a.yml", "enter: !import b.yml\n");
        write(&dir, "b.yml", "exit: !import a.yml\n");

        let err = resolve_text("long: !import a.yml\n", &dir).unwrap_err().to_string();
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
        let expected = crate::input::parse_value(
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
        write(
            &dir,
            "trend.yml",
            "fast: !param FAST\nslow: !param SLOW\n",
        );

        let value = resolve_text(
            "cfg: !import { path: trend.yml, params: { FAST: 5 } }\n",
            &dir,
        )
        .unwrap();
        // Only FAST resolved; SLOW is still a placeholder.
        let params = std::collections::HashMap::from([(
            "SLOW".to_string(),
            Value::from(50),
        )]);
        let value = crate::params::substitute(value, &params).unwrap();
        let expected = crate::input::parse_value("cfg: { fast: 5, slow: 50 }\n").unwrap();
        assert_eq!(value, expected);
    }

    #[test]
    fn inline_params_do_not_eagerly_apply_defaults() {
        // A placeholder with a `default:` and no inline coverage must
        // survive the partial pass — otherwise the outer --params
        // couldn't override its default.
        let dir = tmp_dir("defaults");
        write(
            &dir,
            "x.yml",
            "value: !param { key: FAST, default: 99 }\n",
        );

        // No inline table: default kicks in on the outer pass, --params
        // wins over it.
        let value = resolve_text(
            "cfg: !import { path: x.yml, params: {} }\n",
            &dir,
        )
        .unwrap();
        let params = std::collections::HashMap::from([(
            "FAST".to_string(),
            Value::from(3),
        )]);
        let value = crate::params::substitute(value, &params).unwrap();
        let expected = crate::input::parse_value("cfg: { value: 3 }\n").unwrap();
        assert_eq!(value, expected);

        // Same import, no --params: default applies on the outer pass.
        let value = resolve_text(
            "cfg: !import { path: x.yml, params: {} }\n",
            &dir,
        )
        .unwrap();
        let value = crate::params::substitute(value, &std::collections::HashMap::new()).unwrap();
        let expected = crate::input::parse_value("cfg: { value: 99 }\n").unwrap();
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

        let value = resolve_text(
            "cfg: !import { path: a.yml, params: { FAST: 5 } }\n",
            &dir,
        )
        .unwrap();
        let expected =
            crate::input::parse_value("cfg: { inner: { fast: 5 } }\n").unwrap();
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

        let value = resolve_text(
            "cfg: !import { path: a.yml, params: { FAST: 5 } }\n",
            &dir,
        )
        .unwrap();
        let expected =
            crate::input::parse_value("cfg: { inner: { fast: 99 } }\n").unwrap();
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
        let params = std::collections::HashMap::from([(
            "OUTER".to_string(),
            Value::from(7),
        )]);
        let value = crate::params::substitute(value, &params).unwrap();
        let expected = crate::input::parse_value("cfg: { fast: 7 }\n").unwrap();
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
        let expected =
            crate::input::parse_value("cfg: { score: !value 42 }\n").unwrap();
        assert_eq!(value, expected);
    }

    #[test]
    fn a_document_without_imports_is_returned_verbatim() {
        let dir = tmp_dir("noop");
        let text = "symbol: BTC\nlong:\n  enter: !gt { lhs: close, rhs: !value 10 }\n";
        assert_eq!(
            resolve_text(text, &dir).unwrap(),
            crate::input::parse_value(text).unwrap(),
        );
    }
}
