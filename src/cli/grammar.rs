//! `fugazi grammar` / `fugazi schema` — emit the machine-readable spec grammar
//! to stdout as JSON, for tooling and agents that drive the CLI rather than the
//! library.
//!
//! These are the CLI face of the [`spec::grammar`](crate::spec::grammar)
//! introspection the Python bindings expose (`spec_grammar()`,
//! `spec_json_schema()`, `spec_document_json_schema()`) — the same one authority
//! the descriptor is generated off, so it never drifts from what the parser
//! accepts. Output is **only** JSON on stdout, with no styled header, so it
//! pipes cleanly into `jq` or a consumer; diagnostics go to stderr through the
//! shared error path.

use anyhow::{Context, Result};

use crate::spec::grammar::{spec_document_json_schema, spec_grammar_document, spec_json_schema};

/// `fugazi grammar` — the grammar descriptor, `{ schema_version, tags }`.
pub fn descriptor() -> Result<()> {
    emit(&spec_grammar_document())
}

/// `fugazi schema [--document]` — a JSON Schema (draft 2020-12) for the spec:
/// the single-expression grammar by default, or the whole-document envelope
/// (the five strategy shapes) with `--document`.
pub fn schema(document: bool) -> Result<()> {
    let value = if document {
        spec_document_json_schema()
    } else {
        spec_json_schema()
    };
    emit(&value)
}

/// Pretty-print a JSON value to stdout — the sole output of these commands, so
/// it stays pipeable.
///
/// Writes explicitly rather than via `println!`: these commands emit a large
/// document that's routinely piped into `jq` / `head`, and a consumer that
/// closes the pipe early is normal, not a failure — a bare `println!` would
/// panic on the resulting `BrokenPipe`. Treat it as a clean exit.
fn emit(value: &serde_json::Value) -> Result<()> {
    use std::io::Write;
    let text = serde_json::to_string_pretty(value).context("serializing grammar to JSON")?;
    let mut out = std::io::stdout();
    match writeln!(out, "{text}") {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(e) => Err(e).context("writing grammar to stdout"),
    }
}
