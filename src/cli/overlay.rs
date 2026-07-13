//! `-x` / `--overlay` support for `fugazi get`: parse a set of `column =
//! source-expression` pairs (inline or `@file.yml`), build one indicator per
//! column, and add the results as extra CSV columns.
//!
//! An overlay spec is:
//!
//! * an optional **scope prefix** `SYMBOL[FREQ]:` — either component may be
//!   omitted (`BTCUSDT:`, `[1d]:`), and the whole prefix may be omitted;
//! * then the **overlay body**, one of:
//!   - inline `col=expr[,col=expr,...]`, e.g.
//!     `sma20=!sma { period: 20 },ema50=!ema { period: 50 }`;
//!   - `@file.yml`, a YAML mapping of column name → source expression:
//!
//!     ```yaml
//!     sma20: !sma { period: 20 }
//!     ema50: !ema { period: 50 }
//!     ```
//!
//! With a scope, the overlay only runs for matching `(symbol, interval)` fetches;
//! rows produced by other groups render blanks in that column. Each source
//! expression is the same [`ExprSpec`] YAML surface the strategy parser
//! accepts (`close`, `!sma { period: N }`, `!add { lhs, rhs }`, …) — no separate
//! grammar. `!param { key }` placeholders are resolved from `get --params`
//! before the typed parse, so `--params FAST=20 -x 'ma=!sma { period: !param FAST }'`
//! parameterizes an overlay just like a strategy document.
//!
//! To keep the first output bar's overlays already warmed up, `fugazi get` fetches
//! [`stable_period_for`] extra leading bars before `--since` for each
//! `(symbol, interval)` group (the max `stable_period()` across the overlays
//! that apply to that group), and drops them from the output (unless
//! `--keep-unstable` is set). The bound comes straight from
//! [`Indicator::stable_period`](fugazi::Indicator::stable_period), so it stays
//! correct as new indicators enter the library.

use std::collections::HashMap;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value as Json;

use fugazi::Schema;
use fugazi::indicators::{Book, Position};
use fugazi::sources::Interval;

use crate::dyn_indicator::DynIndicator;
use crate::calendar::{parse_interval, parse_scope_parts};
use crate::input::{self, Source};
use crate::params;
use crate::spec::ExprSpec;

/// Which `(symbol, interval)` fetches an overlay applies to. `None` on either
/// side means "any" — no scope filter at all is `OverlayScope::default()`.
#[derive(Debug, Clone, Default)]
pub struct OverlayScope {
    pub symbol: Option<String>,
    pub interval: Option<Interval>,
}

impl OverlayScope {
    /// Whether this scope covers a given fetch group.
    pub fn matches(&self, symbol: &str, interval: Interval) -> bool {
        self.symbol.as_deref().is_none_or(|s| s == symbol)
            && self.interval.is_none_or(|i| i == interval)
    }
}

/// One overlay column: its output name, source expression, and scope.
#[derive(Debug, Clone)]
pub struct Overlay {
    pub name: String,
    pub spec: ExprSpec,
    pub scope: OverlayScope,
}

impl Overlay {
    /// Build a fresh, live indicator for this overlay against `schema` —
    /// the overlay side channel visible to `!get { key }` references in the
    /// spec. A `get` command runs no strategy, so position-anchored leaves
    /// (`entry`, `peak`, `trough`) read from a stub [`Position`] that never
    /// updates and stay `None` throughout the fetch — a user who wires one
    /// in just gets an empty column.
    ///
    /// `schema` is derived from the source-provided atom stream. For a
    /// `csv:` source it holds the input's non-OHLCV columns (column-typed
    /// by [`crate::csv_source`]); for a remote provider it holds whatever
    /// extras that provider exposes (Binance's `quote_volume`, `n_trades`,
    /// …; Yahoo's `adj_close`) — see each `sources/*.rs` for the specific
    /// vocabulary. An overlay can then reference an existing column
    /// (`!ema { source: !get { key: adj_close } }`); a `!get { key }` on an
    /// unknown key panics at build time with the schema's registered-keys
    /// list.
    pub fn build(&self, schema: &std::sync::Arc<Schema>) -> Box<dyn DynIndicator> {
        // Overlays don't run inside a strategy, so there's no live Position
        // or Book — using them here (`entry`, `peak`, book-anchored sizing)
        // never fires. Pass fresh anchors for signature compatibility.
        self.spec.build(&Position::new(), &Book::new(1.0), schema)
    }
}

/// Parse one or more `--overlay` arguments into a flat list of overlay columns.
///
/// The list keeps every overlay in the order it was defined — no name-dedup —
/// so a later scoped overlay can override an earlier global one for its matching
/// groups while other groups keep the global fallback (see [`active_for`]).
/// The base OHLCV column names are reserved.
///
/// `params` resolves `!param { key }` placeholders inside each overlay
/// expression — the same substitution pass the strategy spec applies (see
/// [`crate::params`]) — so `get --params FAST=20 -x 'ma=!sma { period: !param FAST }'`
/// works exactly like it does on a strategy document. The pass runs on the
/// untyped value tree before it deserializes into an [`ExprSpec`].
pub fn parse_specs(sources: &[Source], params: &HashMap<String, Json>) -> Result<Vec<Overlay>> {
    let mut out: Vec<Overlay> = Vec::new();
    for src in sources {
        let batch = parse_one(src, params).with_context(|| format!("--overlay {}", src.label()))?;
        for overlay in batch {
            reject_reserved_name(&overlay.name)?;
            out.push(overlay);
        }
    }
    Ok(out)
}

/// Unique column names in first-appearance order — the CSV header layout.
pub fn column_names(overlays: &[Overlay]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for o in overlays {
        if !names.iter().any(|n| n == &o.name) {
            names.push(o.name.clone());
        }
    }
    names
}

/// For a single `(symbol, interval)` fetch group, pick the overlay that backs
/// each output column: the **last-defined** overlay whose name matches the
/// column and whose scope covers the group. Returned aligned with
/// [`column_names`] — `None` for a column no scoped overlay covers.
///
/// So a bare `-x ma=…` (global) followed by `-x BTC:ma=…` (BTC-scoped) leaves
/// `ma` backed by the BTC entry for BTC fetches and by the global entry for
/// every other symbol.
pub fn active_for<'a>(
    overlays: &'a [Overlay],
    columns: &[String],
    symbol: &str,
    interval: Interval,
) -> Vec<Option<&'a Overlay>> {
    columns
        .iter()
        .map(|col| {
            overlays
                .iter()
                .rev()
                .find(|o| &o.name == col && o.scope.matches(symbol, interval))
        })
        .collect()
}

/// The maximum warm-up length across the overlays that will actually run for a
/// single `(symbol, interval)` fetch group (i.e. the ones [`active_for`]
/// selects). `fugazi get` fetches this many bars before `--since` per group so
/// those overlays are ready on the first output row.
///
/// The per-overlay figure comes from
/// [`Indicator::stable_period`](fugazi::Indicator::stable_period) on a freshly-
/// built instance — so this stays correct as new indicators land in the
/// library, without a spec-side lookup table to maintain in lockstep.
pub fn stable_period_for(
    overlays: &[Overlay],
    columns: &[String],
    symbol: &str,
    interval: Interval,
    schema: &std::sync::Arc<Schema>,
) -> usize {
    active_for(overlays, columns, symbol, interval)
        .into_iter()
        .flatten()
        .map(|o| o.build(schema).stable_period())
        .max()
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

fn parse_one(source: &Source, params: &HashMap<String, Json>) -> Result<Vec<Overlay>> {
    let text = source.read()?;
    match source {
        Source::File(_) => {
            // A file `Source` still needs the scope prefix parsed off the CLI
            // string — the file is loaded from the path suffix. The `Source`
            // enum has already collapsed `@path` into a file, so a file source
            // arrives here with no prefix. Scope, if any, is handled in
            // `parse_argument` before the `Source` is built.
            parse_file(&text, OverlayScope::default(), params)
        }
        Source::Inline(text) => parse_argument(text, params),
    }
}

/// Parse one whole `--overlay` argument: optional `SYMBOL[FREQ]:` scope prefix
/// followed by either inline pairs or `@file.yml`.
fn parse_argument(text: &str, params: &HashMap<String, Json>) -> Result<Vec<Overlay>> {
    let (scope, body) = split_scope(text)?;
    let body = body.trim();
    if body.is_empty() {
        bail!("overlay spec has an empty body");
    }
    if let Some(path) = body.strip_prefix('@') {
        let path = path.trim();
        if path.is_empty() {
            bail!("overlay spec `@` prefix is missing a file path");
        }
        let file_text = std::fs::read_to_string(path)
            .with_context(|| format!("reading overlay file {path:?}"))?;
        parse_file(&file_text, scope, params)
    } else {
        parse_inline(body, scope, params)
    }
}

/// Split off a leading `SYMBOL[FREQ]:` scope prefix. Returns the scope (empty
/// when no prefix is present) and the remaining body. The `:` is only a
/// separator at bracket depth zero, so a `!sma { source: close, period: 20 }`
/// body without a scope still parses.
fn split_scope(text: &str) -> Result<(OverlayScope, &str)> {
    let mut depth: i32 = 0;
    for (i, ch) in text.char_indices() {
        match ch {
            '{' | '[' => depth += 1,
            '}' | ']' => depth -= 1,
            ':' if depth == 0 => {
                let scope_text = text[..i].trim();
                let body = &text[i + 1..];
                return Ok((parse_scope(scope_text)?, body));
            }
            '=' if depth == 0 => break, // an inline pair started; no scope
            _ => {}
        }
    }
    Ok((OverlayScope::default(), text))
}

/// Parse a scope prefix — `SYMBOL`, `[FREQ]`, `SYMBOL[FREQ]`, or empty.
/// Delegates the bracket-splitting to [`parse_scope_parts`]; only the freq→
/// [`Interval`] conversion is overlay-specific (calendar/costs use
/// [`crate::calendar::Frequency`] instead).
fn parse_scope(text: &str) -> Result<OverlayScope> {
    let (symbol, freq_str) =
        parse_scope_parts(text).map_err(|e| anyhow!("overlay scope: {e}"))?;
    let interval = match freq_str {
        Some(freq) => {
            Some(parse_interval(freq).with_context(|| format!("overlay scope {text:?}"))?)
        }
        None => None,
    };
    Ok(OverlayScope { symbol, interval })
}

/// Parse the inline body: `col=expr[,col=expr,...]`. All overlays parsed here
/// share the same (possibly-empty) scope.
fn parse_inline(
    text: &str,
    scope: OverlayScope,
    params: &HashMap<String, Json>,
) -> Result<Vec<Overlay>> {
    let mut out = Vec::new();
    for term in split_top_commas(text)? {
        let term = term.trim();
        if term.is_empty() {
            continue;
        }
        let (name, expr) = term
            .split_once('=')
            .ok_or_else(|| anyhow!("overlay term {term:?} is missing `=`"))?;
        let name = name.trim();
        if name.is_empty() {
            bail!("overlay term {term:?}: empty column name");
        }
        let spec = parse_expr(expr, params).with_context(|| format!("overlay {name:?}"))?;
        out.push(Overlay {
            name: name.to_string(),
            spec,
            scope: scope.clone(),
        });
    }
    if out.is_empty() {
        bail!("overlay spec is empty");
    }
    Ok(out)
}

/// Parse the file form: a YAML mapping of column name → source expression. All
/// entries share the argument's scope.
fn parse_file(
    text: &str,
    scope: OverlayScope,
    params: &HashMap<String, Json>,
) -> Result<Vec<Overlay>> {
    let value = input::parse_value(text).context("parsing overlay YAML")?;
    let value = params::substitute(value, params).context("overlay `!param` substitution")?;
    let Json::Object(map) = value else {
        bail!("overlay file must be a mapping of column names to source expressions");
    };
    let mut out = Vec::with_capacity(map.len());
    for (name, expr_value) in map {
        if name.is_empty() {
            bail!("overlay file has an empty column name");
        }
        let spec: ExprSpec = serde_json::from_value(expr_value)
            .with_context(|| format!("overlay {name:?}"))?;
        out.push(Overlay {
            name,
            spec,
            scope: scope.clone(),
        });
    }
    if out.is_empty() {
        bail!("overlay file has no entries");
    }
    Ok(out)
}

/// Parse a bare source expression (the RHS of `col=expr`) into a [`ExprSpec`].
fn parse_expr(text: &str, params: &HashMap<String, Json>) -> Result<ExprSpec> {
    let expr = text.trim();
    if expr.is_empty() {
        bail!("empty source expression");
    }
    let value = input::parse_value(expr)?;
    let value = params::substitute(value, params).context("overlay `!param` substitution")?;
    Ok(serde_json::from_value(value)?)
}

/// Split a spec by top-level `,` — respects `{...}` and `[...]` bracket depth so a
/// term like `sma20=!sma { source: close, period: 20 }` stays a single term.
fn split_top_commas(s: &str) -> Result<Vec<&str>> {
    let mut parts = Vec::new();
    let mut depth: i32 = 0;
    let mut start = 0usize;
    for (i, ch) in s.char_indices() {
        match ch {
            '{' | '[' => depth += 1,
            '}' | ']' => {
                depth -= 1;
                if depth < 0 {
                    bail!("unexpected {ch:?} in overlay spec");
                }
            }
            ',' if depth == 0 => {
                parts.push(&s[start..i]);
                start = i + ch.len_utf8();
            }
            _ => {}
        }
    }
    if depth != 0 {
        bail!("unclosed bracket in overlay spec");
    }
    parts.push(&s[start..]);
    Ok(parts)
}

/// Reserved names collide with the base CSV columns `fugazi get` writes.
const RESERVED_COLUMNS: &[&str] = &[
    "symbol", "freq", "time", "open", "high", "low", "close", "volume",
];

fn reject_reserved_name(name: &str) -> Result<()> {
    if RESERVED_COLUMNS.iter().any(|r| r.eq_ignore_ascii_case(name)) {
        bail!("overlay column {name:?} collides with the reserved base column");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Most tests don't exercise `!param`, so wrap the two-arg `parse_specs`
    /// with an empty table — the bare name shadows the `super::*` glob import.
    fn parse_specs(sources: &[Source]) -> Result<Vec<Overlay>> {
        super::parse_specs(sources, &HashMap::new())
    }

    #[test]
    fn parses_inline_multiple_columns_no_scope() {
        let src = Source::Inline(
            "sma20=!sma { period: 20 },ema50=!ema { source: close, period: 50 }".to_string(),
        );
        let overlays = parse_specs(std::slice::from_ref(&src)).unwrap();
        assert_eq!(overlays.len(), 2);
        assert_eq!(overlays[0].name, "sma20");
        assert_eq!(overlays[1].name, "ema50");
        assert!(overlays[0].scope.symbol.is_none());
        assert!(overlays[0].scope.interval.is_none());
    }

    #[test]
    fn parses_inline_bare_word_source() {
        let src = Source::Inline("c=close".to_string());
        let overlays = parse_specs(std::slice::from_ref(&src)).unwrap();
        assert_eq!(overlays.len(), 1);
        assert!(matches!(overlays[0].spec, ExprSpec::Close { .. }));
    }

    #[test]
    fn parses_scope_symbol_and_freq() {
        let src = Source::Inline("BTCUSDT[1d]:s=!sma { period: 5 }".to_string());
        let overlays = parse_specs(std::slice::from_ref(&src)).unwrap();
        assert_eq!(overlays.len(), 1);
        assert_eq!(overlays[0].scope.symbol.as_deref(), Some("BTCUSDT"));
        assert_eq!(overlays[0].scope.interval, Some(Interval::Day(1)));
    }

    #[test]
    fn parses_scope_symbol_only() {
        let src = Source::Inline("BTCUSDT:s=!sma { period: 5 }".to_string());
        let overlays = parse_specs(std::slice::from_ref(&src)).unwrap();
        assert_eq!(overlays[0].scope.symbol.as_deref(), Some("BTCUSDT"));
        assert!(overlays[0].scope.interval.is_none());
    }

    #[test]
    fn parses_scope_freq_only() {
        let src = Source::Inline("[1h]:s=!sma { period: 5 }".to_string());
        let overlays = parse_specs(std::slice::from_ref(&src)).unwrap();
        assert!(overlays[0].scope.symbol.is_none());
        assert_eq!(overlays[0].scope.interval, Some(Interval::Hour(1)));
    }

    #[test]
    fn leading_scope_distributes_over_every_inline_column() {
        // `-x 'BTC[1h]:a=…,b=…,c=…'` — the leading scope covers every column
        // in the same flag. The `PREFIX applies to all Opt1, Opt2, …` invariant
        // this file shares with `--costs` (see costs::tests).
        let src = Source::Inline(
            "BTC[1h]:sma20=!sma { period: 20 },ema50=!ema { period: 50 },c=close".to_string(),
        );
        let overlays = parse_specs(std::slice::from_ref(&src)).unwrap();
        assert_eq!(overlays.len(), 3);
        for o in &overlays {
            assert_eq!(o.scope.symbol.as_deref(), Some("BTC"));
            assert_eq!(o.scope.interval, Some(Interval::Hour(1)));
        }
    }

    #[test]
    fn leading_scope_distributes_over_every_file_entry() {
        // Same invariant for the `@file.yml` body form — every mapping entry
        // inherits the flag's leading scope.
        let path = std::env::temp_dir().join("fugazi_overlay_scope_test.yml");
        std::fs::write(
            &path,
            "sma20: !sma { period: 20 }\nema50: !ema { period: 50 }\n",
        )
        .unwrap();
        let src = Source::Inline(format!("BTC[1h]:@{}", path.display()));
        let overlays = parse_specs(std::slice::from_ref(&src)).unwrap();
        assert_eq!(overlays.len(), 2);
        for o in &overlays {
            assert_eq!(o.scope.symbol.as_deref(), Some("BTC"));
            assert_eq!(o.scope.interval, Some(Interval::Hour(1)));
        }
    }

    #[test]
    fn overlay_scope_matches_wildcards() {
        let scope = OverlayScope {
            symbol: Some("BTC".to_string()),
            interval: None,
        };
        assert!(scope.matches("BTC", Interval::Day(1)));
        assert!(scope.matches("BTC", Interval::Hour(1)));
        assert!(!scope.matches("ETH", Interval::Day(1)));

        let scope = OverlayScope {
            symbol: None,
            interval: Some(Interval::Day(1)),
        };
        assert!(scope.matches("BTC", Interval::Day(1)));
        assert!(!scope.matches("BTC", Interval::Hour(1)));

        let scope = OverlayScope::default();
        assert!(scope.matches("anything", Interval::Minute(5)));
    }

    #[test]
    fn later_same_name_overlay_is_kept_alongside_earlier() {
        // Same name across two `--overlay` args no longer collapses; both are
        // kept so a scoped later one can override the earlier global fallback
        // for its groups without erasing it everywhere.
        let a = Source::Inline("x=!sma { period: 5 }".to_string());
        let b = Source::Inline("BTC:x=!ema { period: 10 }".to_string());
        let overlays = parse_specs(&[a, b]).unwrap();
        assert_eq!(overlays.len(), 2);
        let cols = column_names(&overlays);
        assert_eq!(cols, vec!["x".to_string()]);
    }

    #[test]
    fn active_for_picks_last_matching_scope() {
        // Global `x=SMA` + BTC-scoped `x=EMA`. BTC should see the EMA, other
        // symbols should fall back to the global SMA.
        let a = Source::Inline("x=!sma { period: 5 }".to_string());
        let b = Source::Inline("BTC:x=!ema { period: 10 }".to_string());
        let overlays = parse_specs(&[a, b]).unwrap();
        let cols = column_names(&overlays);
        let btc = active_for(&overlays, &cols, "BTC", Interval::Day(1));
        assert!(matches!(btc[0].map(|o| &o.spec), Some(ExprSpec::Ema { .. })));
        let eth = active_for(&overlays, &cols, "ETH", Interval::Day(1));
        assert!(matches!(eth[0].map(|o| &o.spec), Some(ExprSpec::Sma { .. })));
    }

    #[test]
    fn rejects_reserved_column_name() {
        let src = Source::Inline("close=!sma { period: 5 }".to_string());
        assert!(parse_specs(std::slice::from_ref(&src)).is_err());
    }

    #[test]
    fn rejects_missing_equals_in_inline() {
        let src = Source::Inline("!sma { period: 5 }".to_string());
        assert!(parse_specs(std::slice::from_ref(&src)).is_err());
    }

    #[test]
    fn colon_inside_indicator_body_is_not_a_scope_separator() {
        // `!sma { source: close, period: 20 }` contains a colon inside `{...}`.
        // That colon is at bracket depth 1, so it must not be mistaken for the
        // scope separator.
        let src = Source::Inline("s=!sma { source: close, period: 20 }".to_string());
        let overlays = parse_specs(std::slice::from_ref(&src)).unwrap();
        assert_eq!(overlays.len(), 1);
        assert!(overlays[0].scope.symbol.is_none());
    }

    #[test]
    fn stable_period_for_only_counts_applicable_overlays() {
        let overlays = vec![
            Overlay {
                name: "a".to_string(),
                spec: ExprSpec::Sma {
                    source: Box::new(ExprSpec::Close { source: None }),
                    period: 200,
                },
                scope: OverlayScope {
                    symbol: Some("BTC".to_string()),
                    interval: None,
                },
            },
            Overlay {
                name: "b".to_string(),
                spec: ExprSpec::Sma {
                    source: Box::new(ExprSpec::Close { source: None }),
                    period: 20,
                },
                scope: OverlayScope::default(),
            },
        ];
        let cols = column_names(&overlays);
        assert_eq!(stable_period_for(&overlays, &cols, "BTC", Interval::Day(1), &Schema::empty()), 200);
        assert_eq!(stable_period_for(&overlays, &cols, "ETH", Interval::Day(1), &Schema::empty()), 20);
    }

    #[test]
    fn stable_period_uses_active_override_not_the_shadowed_global() {
        // Global `ma=SMA(200)` shadowed for BTC by `ma=SMA(30)`. BTC's warm-up
        // must reflect the BTC override (30), not the shadowed 200.
        let a = Source::Inline("ma=!sma { period: 200 }".to_string());
        let b = Source::Inline("BTC:ma=!sma { period: 30 }".to_string());
        let overlays = parse_specs(&[a, b]).unwrap();
        let cols = column_names(&overlays);
        assert_eq!(stable_period_for(&overlays, &cols, "BTC", Interval::Day(1), &Schema::empty()), 30);
        assert_eq!(stable_period_for(&overlays, &cols, "ETH", Interval::Day(1), &Schema::empty()), 200);
    }

    #[test]
    fn stable_period_derives_from_library() {
        // Sanity check: the value comes straight from Indicator::stable_period()
        // on the freshly-built DynValue.
        let src = Source::Inline("s=!sma { period: 14 }".to_string());
        let overlays = parse_specs(std::slice::from_ref(&src)).unwrap();
        let cols = column_names(&overlays);
        assert_eq!(stable_period_for(&overlays, &cols, "X", Interval::Day(1), &Schema::empty()), 14);
    }

    #[test]
    fn param_substitutes_in_inline_overlay() {
        // `!param FAST` inside an inline overlay expression resolves from the
        // `--params` table before the typed `ExprSpec` parse, exactly as it
        // does in a strategy document.
        let src = Source::Inline("ma=!sma { period: !param FAST }".to_string());
        let table = HashMap::from([("FAST".to_string(), Json::from(20))]);
        let overlays = super::parse_specs(std::slice::from_ref(&src), &table).unwrap();
        assert_eq!(overlays.len(), 1);
        assert!(matches!(
            overlays[0].spec,
            ExprSpec::Sma { period: 20, .. }
        ));
    }

    #[test]
    fn param_default_applies_when_unset() {
        let src = Source::Inline("ma=!sma { period: !param { key: FAST, default: 14 } }".to_string());
        let overlays = super::parse_specs(std::slice::from_ref(&src), &HashMap::new()).unwrap();
        assert!(matches!(
            overlays[0].spec,
            ExprSpec::Sma { period: 14, .. }
        ));
    }

    #[test]
    fn missing_param_without_default_errors() {
        let src = Source::Inline("ma=!sma { period: !param FAST }".to_string());
        let err = super::parse_specs(std::slice::from_ref(&src), &HashMap::new()).unwrap_err();
        assert!(format!("{err:#}").contains("FAST"));
    }

    #[test]
    fn param_substitutes_in_file_overlay() {
        let path = std::env::temp_dir().join("fugazi_overlay_param_test.yml");
        std::fs::write(&path, "ma: !sma { period: !param FAST }\n").unwrap();
        let src = Source::Inline(format!("@{}", path.display()));
        let table = HashMap::from([("FAST".to_string(), Json::from(30))]);
        let overlays = super::parse_specs(std::slice::from_ref(&src), &table).unwrap();
        assert!(matches!(
            overlays[0].spec,
            ExprSpec::Sma { period: 30, .. }
        ));
    }
}
