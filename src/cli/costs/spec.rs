//! `--costs` argument parsing: `CostSpec` + one term.
//!
//! Split out of `costs/mod.rs`; kept in `crate::costs::spec` so paths like
//! `crate::costs::CostSpec` still resolve via the `pub use` in `mod.rs`.

use std::str::FromStr;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;

use crate::calendar::{self, Scope};
use crate::input::{self, Source};

// ---------------------------------------------------------------------------
// Parsing: one `--costs` argument
// ---------------------------------------------------------------------------

/// (`@file`, `[SCOPE:]key=value`, or the literal `none`) applied in order.
#[derive(Debug, Clone)]
pub struct CostSpec(pub(super) Vec<CostTerm>);

/// One term of a `--costs` spec.
#[derive(Debug, Clone)]
pub(super) enum CostTerm {
    /// `@file.yml` — a whole cost-model preset.
    Load(Source),
    /// `[SCOPE:]key=value` — a single leaf-or-model setter.
    Set {
        scope: Scope,
        key: Vec<String>,
        value: Value,
    },
    /// The literal `none` — reset every leg to its no-op default. Doubles as the
    /// user's way of silencing the "no cost model set" warning banner without
    /// activating a model.
    None,
}

impl FromStr for CostSpec {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut terms = Vec::new();
        for chunk in split_top_commas(s).map_err(|e| e.to_string())? {
            let chunk = chunk.trim();
            if chunk.is_empty() {
                continue;
            }
            terms.push(parse_term(chunk).map_err(|e| e.to_string())?);
        }
        Ok(CostSpec(terms))
    }
}

/// Parse one term: `@file`, `none`, or `[SCOPE:]key=value`.
fn parse_term(text: &str) -> Result<CostTerm> {
    if text == "none" {
        return Ok(CostTerm::None);
    }
    if let Some(path) = text.strip_prefix('@') {
        let path = path.trim();
        if path.is_empty() {
            bail!("`@` prefix is missing a file path");
        }
        // Source::from_str strips '@' — we've already stripped it, so build the
        // File variant directly to avoid a round-trip that would classify it
        // as inline text.
        return Ok(CostTerm::Load(Source::File(path.into())));
    }
    let (scope, body) = split_scope(text)?;
    let body = body.trim();
    if body.is_empty() {
        bail!("cost term has an empty body");
    }
    let (key_str, value_str) = body
        .split_once('=')
        .ok_or_else(|| anyhow!("cost term {body:?} is missing `=`"))?;
    let key: Vec<String> = key_str.trim().split('.').map(String::from).collect();
    if key.iter().any(String::is_empty) {
        bail!("cost term key {key_str:?}: empty segment");
    }
    if !matches!(key[0].as_str(), "commission" | "spread" | "slippage") {
        bail!(
            "cost term key {key_str:?}: first segment must be one of \
             `commission`, `spread`, `slippage` (got `{}`)",
            key[0]
        );
    }
    let value = input::parse_value(value_str.trim())
        .with_context(|| format!("parsing cost value for `{key_str}`"))?;
    Ok(CostTerm::Set { scope, key, value })
}

// ---------------------------------------------------------------------------
// Scope
// ---------------------------------------------------------------------------

/// Split off a leading `SYMBOL[FREQ]:` scope prefix. Same bracket grammar as
/// [`crate::calendar::parse_scope`], but the delimiter rules are cost-DSL
/// specific: a `:` at bracket depth zero is the separator; a `=` at depth zero
/// without a preceding `:` means "no scope, start of an inline pair" — so the
/// splitter can't share with the calendar side, which never sees `=`.
fn split_scope(text: &str) -> Result<(Scope, &str)> {
    let mut depth: i32 = 0;
    for (i, ch) in text.char_indices() {
        match ch {
            '{' | '[' => depth += 1,
            '}' | ']' => depth -= 1,
            ':' if depth == 0 => {
                let scope_text = text[..i].trim();
                let body = &text[i + 1..];
                let scope = calendar::parse_scope(scope_text)
                    .map_err(|e| anyhow!("cost scope: {e}"))?;
                return Ok((scope, body));
            }
            '=' if depth == 0 => break,
            _ => {}
        }
    }
    Ok((Scope::default(), text))
}

// ---------------------------------------------------------------------------
// Top-level `,`-splitter — respects `{...}` / `[...]` / `"..."` grouping so a
// term like `commission=!percentage { rate: 0.001 }` stays a single term.
// ---------------------------------------------------------------------------

fn split_top_commas(s: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut depth: i32 = 0;
    let mut in_str = false;
    let mut prev = '\0';
    for c in s.chars() {
        if in_str {
            buf.push(c);
            if c == '"' && prev != '\\' {
                in_str = false;
            }
        } else {
            match c {
                '"' => {
                    in_str = true;
                    buf.push(c);
                }
                '{' | '[' => {
                    depth += 1;
                    buf.push(c);
                }
                '}' | ']' => {
                    depth -= 1;
                    if depth < 0 {
                        bail!("unexpected `{c}` in cost spec");
                    }
                    buf.push(c);
                }
                ',' if depth == 0 => {
                    out.push(std::mem::take(&mut buf));
                }
                _ => buf.push(c),
            }
        }
        prev = c;
    }
    if depth != 0 {
        bail!("unclosed bracket in cost spec");
    }
    if !buf.is_empty() {
        out.push(buf);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Folding: multiple CostSpec → one Value tree → typed CostConfig
// ---------------------------------------------------------------------------

