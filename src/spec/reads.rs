//! Which **series a document reads but does not trade** — the symbols named by
//! an explicit `!pick { symbol: … }` anywhere in the tree.
//!
//! A run's snapshot stream carries exactly the series the document uses. Two
//! halves make up "uses": the symbols the shape *trades* (declared by
//! `symbol:` / `left:` / `right:` / a portfolio's children, or discovered from
//! the frame for basket and multi-asset), and the symbols it merely *reads* —
//! a regime gate gets one asset's trend from another's:
//!
//! ```yaml
//! symbol: ETHUSDT
//! long:
//!   enter: !gt
//!     lhs: !close { source: !pick { symbol: BTCUSDT } }
//!     rhs: !sma { period: 200, source: !close { source: !pick { symbol: BTCUSDT } } }
//! ```
//!
//! The traded half every runner already knew. This module supplies the read
//! half, so the CLI runners can join `BTCUSDT`'s bars into `ETHUSDT`'s
//! snapshots — and refuse the run outright when the named series isn't in the
//! input, rather than resolving `None` on every bar and reporting a plausible
//! backtest that never trades.
//!
//! # Why this walks the loaded document rather than the typed spec
//!
//! [`NodeSpec`](super::expr::NodeSpec) has ~142 variants and no generic child
//! iterator; a typed walk would be a second 142-arm match that a new tag with a
//! nested slot could silently fall out of — reintroducing exactly the silent
//! failure this exists to prevent. The loaded document is a
//! [`serde_json::Value`] with every `!tag` in serde's singleton-map form
//! (`!pick { symbol: BTC }` → `{"pick": {"symbol": "BTC"}}`, see
//! [`convert::yaml_to_json`](super::convert::yaml_to_json)), so a *structural*
//! walk visits every nested expression unconditionally, whatever tag encloses
//! it. `!pick` is the only tag that names an asset inside an expression, so the
//! tag name is the whole contract.
//!
//! The walk runs after `!import` splicing and `!param` substitution, so an
//! imported subtree and a parameterised symbol are both seen. A `symbol:` still
//! holding a hole — `!arg SYM` in a basket / multi-asset / portfolio template,
//! or an unresolved `!param` under `check` — is not a string and is skipped:
//! a template's per-leg symbol is supplied at build time from the universe,
//! which is in the frame by construction.

use std::collections::BTreeSet;

/// Every symbol named by an explicit `!pick { symbol: … }` in `doc`, in sorted
/// order.
///
/// `doc` is a loaded strategy document — the [`serde_json::Value`] that
/// [`load_value`](super::load_value) returns. Symbols are returned **verbatim**,
/// matching what [`build_pick`](super::expr) interns: only the *scope* grammars
/// (`SYMBOL[FREQ]:` prefixes) carry the `\:` escape, and a `!pick` head is not
/// one of them.
///
/// Includes symbols the shape also trades — deduplicating against the traded
/// set is the caller's job, and the runners want the union anyway.
pub fn picked_symbols(doc: &serde_json::Value) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    collect(doc, &mut out);
    out
}

/// Load `text` the way the shape loaders do — `!import`s spliced, `!param`s
/// substituted — and report the symbols it reads through `!pick`.
///
/// This re-runs [`load_value`](super::load_value) rather than sharing the load
/// with the caller's `*Spec::from_text_with_params_in`. That is deliberate: the
/// alternative is a second constructor on all five shape types (plus
/// [`StrategyRef`](super::preset::StrategyRef)) threading a set nobody but the
/// CLI wants, and the cost here is one more parse of a document already in
/// memory.
pub fn picked_symbols_of(
    text: &str,
    params: &std::collections::HashMap<String, serde_json::Value>,
    base: &std::path::Path,
    label: &str,
) -> anyhow::Result<BTreeSet<String>> {
    Ok(picked_symbols(&super::load_value(
        text, params, base, label,
    )?))
}

/// [`picked_streams`] for a caller holding the document text — the twin of
/// [`picked_symbols_of`], loaded the same way.
pub fn picked_streams_of(
    text: &str,
    params: &std::collections::HashMap<String, serde_json::Value>,
    base: &std::path::Path,
    label: &str,
) -> anyhow::Result<BTreeSet<String>> {
    Ok(picked_streams(&super::load_value(
        text, params, base, label,
    )?))
}

/// [`picked_symbols_of`] for a caller that disables `!import` (see
/// [`load_value_no_imports`](super::load_value_no_imports)).
pub fn picked_symbols_of_no_imports(
    text: &str,
    params: &std::collections::HashMap<String, serde_json::Value>,
    label: &str,
) -> anyhow::Result<BTreeSet<String>> {
    Ok(picked_symbols(&super::load_value_no_imports(
        text, params, label,
    )?))
}

/// The document keys that name a series the shape **trades**: a single-asset or
/// portfolio-child `root:`, and a pair's two legs.
///
/// Skipped by [`collect`], because this walk answers "what does the document
/// *read*" and a traded series is not a read-only one. It mattered less when
/// those keys held plain strings — the walk only ever looked at `!pick` bodies,
/// so they fell out for free. Now that a root *is* an expression, and usually a
/// `!pick`, excluding them has to be said out loud.
///
/// Safe to apply at any depth rather than only at the document root: no
/// expression tag has a field by any of these names (the binary operators spell
/// theirs `lhs`/`rhs`), so a nested match is always a document key — which is
/// exactly what a portfolio child is.
const TRADED_KEYS: [&str; 3] = ["root", "left", "right"];

/// Recurse structurally, recording the `symbol:` of every `!pick` on the way
/// down. A `!pick` node is descended into as well — its `symbol:` is a scalar,
/// but nesting is the tree's business, not this walk's.
fn collect(value: &serde_json::Value, out: &mut BTreeSet<String>) {
    collect_field(value, "symbol", true, out)
}

/// Every **stream** a document names — the `freq:` of every `!pick`.
///
/// The twin of [`picked_symbols`], and it exists for the same reason. A stream
/// id is opaque now, so nothing parses it and nothing would notice a typo:
/// `!pick { symbol: BTC, freq: 1hh }` builds happily and then matches no entry
/// on any bar, and the run reports a plausible zero-fill backtest. That is the
/// exact failure this crate goes out of its way to make impossible, and
/// checking the named stream against the input is what replaces the cadence
/// parse that used to catch a subset of it — a strictly wider net, since it
/// also catches a perfectly well-formed `1d` against an hourly-only input.
///
/// Unlike [`picked_symbols`] this does **not** skip the traded keys: a traded
/// series is not a "read", but a stream named on a `root:` still has to exist,
/// and there is no separate check that would catch it.
pub fn picked_streams(doc: &serde_json::Value) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    collect_field(doc, "freq", false, &mut out);
    collect_field(doc, "stream", false, &mut out);
    out
}

/// Shared walk: record `!pick`'s `field` everywhere it appears.
///
/// `skip_traded` is what separates the two callers — see [`picked_streams`].
fn collect_field(
    value: &serde_json::Value,
    field: &str,
    skip_traded: bool,
    out: &mut BTreeSet<String>,
) {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(serde_json::Value::Object(body)) = map.get("pick")
                && let Some(serde_json::Value::String(found)) = body.get(field)
            {
                out.insert(found.clone());
            }
            for (k, v) in map {
                if skip_traded && TRADED_KEYS.contains(&k.as_str()) {
                    continue;
                }
                collect_field(v, field, skip_traded, out);
            }
        }
        serde_json::Value::Array(items) => {
            for v in items {
                collect_field(v, field, skip_traded, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picked(text: &str) -> Vec<String> {
        let doc = super::super::load_value(
            text,
            &Default::default(),
            std::path::Path::new("."),
            "(test)",
        )
        .expect("document loads");
        picked_symbols(&doc).into_iter().collect()
    }

    fn streams(text: &str) -> Vec<String> {
        let doc = super::super::load_value(
            text,
            &Default::default(),
            std::path::Path::new("."),
            "(test)",
        )
        .expect("document loads");
        picked_streams(&doc).into_iter().collect()
    }

    /// The guardrail that replaces the cadence parse: every named stream is
    /// collected so the runner can check it against the input.
    #[test]
    fn finds_every_named_stream() {
        let found = streams(
            r#"
root: !pick { symbol: ETH, freq: 1h }
long:
  enter: !gt
    lhs: !close { source: !pick { symbol: BTC, freq: dollar-1e6 } }
    rhs: !value 0
"#,
        );
        assert_eq!(found, vec!["1h".to_string(), "dollar-1e6".to_string()]);
    }

    /// Unlike `picked_symbols`, this does **not** skip the traded keys: a
    /// stream named on `root:` still has to exist, and nothing else checks it.
    #[test]
    fn a_stream_on_the_root_is_not_skipped() {
        assert_eq!(
            streams("root: !pick { symbol: BTC, freq: 4h }\nlong:\n  enter: !value true\n"),
            vec!["4h".to_string()]
        );
    }

    /// A document naming no stream at all yields none — every document that
    /// worked before this existed.
    #[test]
    fn no_named_stream_is_an_empty_set() {
        assert!(streams("root: BTC\nlong:\n  enter: !value true\n").is_empty());
    }

    #[test]
    fn finds_a_pick_nested_under_a_leaf() {
        let syms = picked(
            r#"
root: ETH
long:
  enter: !gt { lhs: !close { source: !pick { symbol: BTC } }, rhs: !value 0 }
"#,
        );
        assert_eq!(syms, vec!["BTC".to_string()]);
    }

    /// The document's own `symbol:` is not a `!pick`, and neither is a
    /// `symbol:` on any other tag — only the one nested inside `!pick` counts.
    #[test]
    fn ignores_a_symbol_key_outside_a_pick() {
        let syms = picked(
            r#"
root: ETH
long:
  enter: !gt { lhs: !close, rhs: !value 0 }
"#,
        );
        assert!(syms.is_empty(), "got {syms:?}");
    }

    #[test]
    fn dedupes_and_sorts_across_the_whole_tree() {
        let syms = picked(
            r#"
root: ETH
long:
  enter: !gt { lhs: !close { source: !pick { symbol: SOL } }, rhs: !value 0 }
  exit: !lt { lhs: !close { source: !pick { symbol: BTC } }, rhs: !value 0 }
sizing: !div
  lhs: !atr { period: 14, source: !current { source: !pick { symbol: BTC } } }
  rhs: !value 100
"#,
        );
        assert_eq!(syms, vec!["BTC".to_string(), "SOL".to_string()]);
    }

    /// A freq-only `!pick` names no asset, and a bare `!pick` has no body at
    /// all — neither may fabricate an entry.
    #[test]
    fn a_pick_with_no_symbol_contributes_nothing() {
        let syms = picked(
            r#"
root: ETH
long:
  enter: !gt { lhs: !close { source: !pick { freq: 1d } }, rhs: !value 0 }
  exit: !lt { lhs: !close { source: !pick }, rhs: !value 0 }
"#,
        );
        assert!(syms.is_empty(), "got {syms:?}");
    }

    /// A template's per-leg symbol is a hole at load time, not a string. It is
    /// filled from the traded universe at build time, so it is already in the
    /// frame and there is nothing to join in.
    #[test]
    fn an_arg_hole_is_not_a_symbol() {
        let syms = picked(
            r#"
basket:
  select: !top_bottom { top: 1 }
  score: !rsi { period: 14, source: !close { source: !pick { symbol: !arg SYM } } }
"#,
        );
        assert!(syms.is_empty(), "got {syms:?}");
    }
}
