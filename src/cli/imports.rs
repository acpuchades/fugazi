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

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};

/// The singleton key a `!import` tag normalizes to (see [`crate::convert`]).
const IMPORT: &str = "import";

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
            if let Some(path) = import_path(&map)? {
                return load(&path, base, stack);
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

/// The path an `!import` node names, or `None` when `map` isn't one: a
/// singleton object keyed `import` whose body is the path string. A non-string
/// body is an error — `!import` takes a file, and a map body would otherwise be
/// mistaken for a spec fragment and fail much later with a confusing type error.
fn import_path(map: &Map<String, Value>) -> Result<Option<String>> {
    if map.len() != 1 {
        return Ok(None);
    }
    let Some(body) = map.get(IMPORT) else {
        return Ok(None);
    };
    match body {
        Value::String(path) => Ok(Some(path.clone())),
        other => bail!(
            "!import takes the path of a YAML file to splice in \
             (`!import signals/breakout.yml`), got {other}"
        ),
    }
}

/// Load one imported document: read it relative to `base`, parse it (its own
/// `!tag`s normalize exactly like the importing document's), and resolve its
/// nested imports against *its own* directory.
fn load(path: &str, base: &Path, stack: &mut Vec<PathBuf>) -> Result<Value> {
    let joined = base.join(path);
    let canonical = std::fs::canonicalize(&joined)
        .with_context(|| format!("!import {path}: reading `{}`", joined.display()))?;

    if let Some(start) = stack.iter().position(|seen| *seen == canonical) {
        let chain: Vec<String> = stack[start..]
            .iter()
            .chain(std::iter::once(&canonical))
            .map(|p| p.display().to_string())
            .collect();
        bail!("!import cycle: {}", chain.join(" -> "));
    }

    let text = std::fs::read_to_string(&canonical)
        .with_context(|| format!("!import {path}: reading `{}`", canonical.display()))?;
    let value = crate::input::parse_value(&text)
        .with_context(|| format!("!import {path}: parsing `{}`", canonical.display()))?;

    let dir = canonical
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    stack.push(canonical);
    let resolved = walk(value, &dir, stack);
    stack.pop();
    resolved
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
    fn a_non_string_body_is_rejected() {
        let dir = tmp_dir("body");
        let err = resolve_text("enter: !import { period: 3 }\n", &dir)
            .unwrap_err()
            .to_string();
        assert!(err.contains("!import takes the path"), "{err}");
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
