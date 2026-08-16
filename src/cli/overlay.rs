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
//! expression is the same [`NodeSpec`] YAML surface the strategy parser
//! accepts (`close`, `!sma { period: N }`, `!add { lhs, rhs }`, …) — no separate
//! grammar. `!param { key }` placeholders are resolved from `get --params`
//! before the typed parse, so `--params FAST=20 -x 'ma=!sma { period: !param FAST }'`
//! parameterizes an overlay just like a strategy document.
//!
//! To keep the first output bar's overlays already warmed up, `fugazi get` fetches
//! [`stable_bars_for`] extra leading bars before `--since` for each
//! `(symbol, interval)` group (the max `stable_bars()` across the overlays
//! that apply to that group), and drops them from the output (unless
//! `--keep-unstable` is set). The bound comes straight from
//! [`Indicator::stable_bars`](fugazi::Indicator::stable_bars), so it stays
//! correct as new indicators enter the library.

use std::collections::HashMap;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value as Json;

use std::str::FromStr;

use fugazi::sources::Interval;
use fugazi::{Frequency, Schema, Selector};

use crate::dyn_indicator::DynIndicator;
use crate::calendar::{is_escaped, looks_like_body, parse_interval, parse_scope_parts};
use crate::input::{self, Source};
use crate::params;
use crate::spec::NodeSpec;

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

/// One overlay column: its output name, source expression, scope, and the
/// document it came from (used to name the offending file in a build error).
#[derive(Debug, Clone)]
pub struct Overlay {
    pub name: String,
    pub spec: NodeSpec,
    pub scope: OverlayScope,
    pub origin: String,
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
    /// unknown key is a build error listing the schema's registered keys.
    /// `root` is the blessed series this instance reads for any
    /// `source:`-omitted leaf — the `(symbol, freq)` fetch group it's being
    /// built for. A bare `!close` reads that group's own bar;
    /// `!close { source: !pick { symbol: SPY } }` reaches across to SPY in the
    /// same snapshot.
    ///
    /// Returns `Err` rather than aborting when the expression can't be built
    /// (an unknown `!get` key, a malformed `!pick` frequency, …) — see
    /// [`crate::spec::overlay::build_overlay`]. The message names this column
    /// and the document it was written in.
    pub fn build(
        &self,
        schema: &std::sync::Arc<Schema>,
        root: Option<&Selector<String>>,
    ) -> Result<Box<dyn DynIndicator>> {
        // Overlays don't run inside a strategy, so there's no live Position
        // or Book — using them here (`entry`, `peak`, book-anchored sizing)
        // never fires. The shared library core installs the stub anchors.
        let built = crate::spec::overlay::build_overlay(&self.spec, schema, root)
            .map_err(|e| anyhow!("overlay {:?} in {}: {e}", self.name, self.origin))?;
        // A column that emits an `Atom` or a `Candle` has no CSV cell to
        // widen into. Reject it here, where the column name and origin are
        // in hand — otherwise it reaches `dyn_value_to_overlay` mid-stream
        // and there is no error path left to return through.
        crate::spec::overlay::scalar_type(built.as_ref(), &self.name)
            .map_err(|e| anyhow!("{e} (in {})", self.origin))?;
        Ok(built)
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
/// untyped value tree before it deserializes into an [`NodeSpec`].
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
/// [`Indicator::stable_bars`](fugazi::Indicator::stable_bars) on a freshly-
/// built instance — so this stays correct as new indicators land in the
/// library, without a spec-side lookup table to maintain in lockstep.
pub fn stable_bars_for(
    overlays: &[Overlay],
    columns: &[String],
    symbol: &str,
    interval: Interval,
    schema: &std::sync::Arc<Schema>,
) -> Result<usize> {
    let root = group_root(symbol, interval);
    let mut max = 0usize;
    for o in active_for(overlays, columns, symbol, interval)
        .into_iter()
        .flatten()
    {
        max = max.max(o.build(schema, Some(&root))?.stable_bars());
    }
    Ok(max)
}

/// The blessed series of one `(symbol, interval)` fetch group, as a
/// [`Selector`] the overlay build roots its `source:`-omitted leaves on.
///
/// The interval round-trips through its token (`"1d"`) because
/// [`Interval`] and [`Frequency`] are provider-side and library-side twins with
/// no direct conversion; `as_token` / `from_str` are the existing bridge, and
/// the same token is what the emitted `freq` column spells. A token that
/// somehow doesn't parse degrades to a symbol-only selector rather than
/// failing the fetch — matching on symbol alone is still right whenever a
/// symbol appears at one cadence, which is the overwhelmingly common case.
pub fn group_root(symbol: &str, interval: Interval) -> Selector<String> {
    Selector::<String> {
        symbol: Some(symbol.to_string()),
        freq: Frequency::from_str(&interval.as_token()).ok(),
    }
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
            parse_file(&text, OverlayScope::default(), params, &source.label())
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
        parse_file(&file_text, scope, params, path)
    } else {
        parse_inline(body, scope, params)
    }
}

/// Split off a leading `SYMBOL[FREQ]:` scope prefix. Returns the scope (empty
/// when no prefix is present) and the remaining body. The `:` is only a
/// separator at bracket depth zero, so a `!sma { source: close, period: 20 }`
/// body without a scope still parses.
///
/// An **escaped** `\=` is not the start of an inline pair — it belongs to the
/// scoped symbol, so `'EURUSD\=X[1d]:r=!rsi { period: 2 }'` scopes to
/// `EURUSD=X` rather than reading `EURUSD` as a column name. Same rule as
/// `fugazi get`'s spec heads; see [`crate::calendar::unescape_symbol`].
fn split_scope(text: &str) -> Result<(OverlayScope, &str)> {
    let mut depth: i32 = 0;
    for (i, ch) in text.char_indices() {
        match ch {
            '{' | '[' => depth += 1,
            '}' | ']' => depth -= 1,
            ':' if depth == 0 && !is_escaped(text, i) => {
                // The prefix is a scope only when what follows can be a
                // body; otherwise this colon belongs to the body itself.
                if !looks_like_body(&text[i + 1..]) {
                    break;
                }
                let scope_text = text[..i].trim();
                let body = &text[i + 1..];
                return Ok((parse_scope(scope_text)?, body));
            }
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
            origin: INLINE_ORIGIN.to_string(),
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
    label: &str,
) -> Result<Vec<Overlay>> {
    let value = input::parse_value_at(text, label)?;
    let value = params::substitute(value, params)
        .with_context(|| format!("resolving `!param` in overlay {label}"))?;
    let out = scoped_from_value(value, scope, label)
        .with_context(|| format!("building overlays from {label}"))?;
    if out.is_empty() {
        bail!("overlay file {label} has no entries");
    }
    Ok(out)
}

/// How an inline `-x col=expr` overlay names itself in a build error.
const INLINE_ORIGIN: &str = "(inline overlay)";

/// Parse a bare source expression (the RHS of `col=expr`) into a [`NodeSpec`].
fn parse_expr(text: &str, params: &HashMap<String, Json>) -> Result<NodeSpec> {
    let expr = text.trim();
    if expr.is_empty() {
        bail!("empty source expression");
    }
    let value = input::parse_value_at(expr, "(inline overlay)")?;
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

/// Parse overlay columns from an already-converted JSON value (a mapping of column
/// names to NodeSpec values).
///
/// The caller is responsible for the `serde_norway → imports → yaml_to_json` pass
/// before calling this — it is used by the dataset YAML parser, which has already
/// done that work. The scope of every column produced here is the default (no
/// symbol or interval filter), i.e. the overlay applies to all fetch groups.
pub fn parse_from_value(
    value: Json,
    params: &HashMap<String, Json>,
    label: &str,
) -> Result<Vec<Overlay>> {
    let value = crate::params::substitute(value, params)
        .with_context(|| format!("resolving `!param` in overlay {label}"))?;
    scoped_from_value(value, OverlayScope::default(), label)
}

/// Shared adapter: parse a `name: NodeSpec` map via the library core, apply the
/// CLI's reserved-name policy, and tag every column with `scope`.
fn scoped_from_value(value: Json, scope: OverlayScope, label: &str) -> Result<Vec<Overlay>> {
    crate::spec::overlay::columns_from_value(value, label)?
        .into_iter()
        .map(|c| {
            reject_reserved_name(&c.name)?;
            Ok(Overlay {
                name: c.name,
                spec: c.spec,
                scope: scope.clone(),
                origin: c.origin,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Period fields are `NonZeroUsize`, so a literal needs wrapping to build
    /// a `NodeSpec` by hand.
    fn nz(n: usize) -> std::num::NonZeroUsize {
        std::num::NonZeroUsize::new(n).expect("test period is non-zero")
    }

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
        assert!(matches!(overlays[0].spec, NodeSpec::Close { .. }));
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
    fn scope_symbol_may_carry_an_escaped_colon() {
        // `:` ends the scope, so a symbol containing one escapes it — CCXT
        // spells a perpetual `BTC/USDT:USDT`.
        let src = Source::Inline(r"BTC/USDT\:USDT[1d]:r=!rsi { period: 2 }".to_string());
        let overlays = parse_specs(std::slice::from_ref(&src)).unwrap();
        assert_eq!(overlays[0].scope.symbol.as_deref(), Some("BTC/USDT:USDT"));
        assert_eq!(overlays[0].scope.interval, Some(Interval::Day(1)));
        assert_eq!(overlays[0].name, "r");
    }

    #[test]
    fn a_scope_symbol_carrying_an_equals_needs_no_escape() {
        // With the remap gone nothing in a symbol is `=`-delimited, so Yahoo's
        // tickers scope plainly. What disambiguates `EURUSD=X:r=…` from a
        // scopeless body is that the text after the colon assigns.
        let src = Source::Inline("EURUSD=X[1d]:r=!rsi { period: 2 }".to_string());
        let overlays = parse_specs(std::slice::from_ref(&src)).unwrap();
        assert_eq!(overlays[0].scope.symbol.as_deref(), Some("EURUSD=X"));
        assert_eq!(overlays[0].name, "r");
    }

    #[test]
    fn a_colon_inside_a_value_does_not_start_a_scope() {
        // `30` assigns nothing, so the colon belongs to the value.
        let src = Source::Inline("dur=!value 12:30".to_string());
        let overlays = parse_specs(std::slice::from_ref(&src)).unwrap();
        assert!(overlays[0].scope.symbol.is_none());
        assert_eq!(overlays[0].name, "dur");
    }

    #[test]
    fn an_unescaped_equals_still_means_no_scope() {
        // The inline-pair form is unchanged: no `:` before the `=`, no scope.
        let src = Source::Inline("s=!sma { period: 5 }".to_string());
        let overlays = parse_specs(std::slice::from_ref(&src)).unwrap();
        assert!(overlays[0].scope.symbol.is_none());
        assert_eq!(overlays[0].name, "s");
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
        assert!(matches!(btc[0].map(|o| &o.spec), Some(NodeSpec::Ema { .. })));
        let eth = active_for(&overlays, &cols, "ETH", Interval::Day(1));
        assert!(matches!(eth[0].map(|o| &o.spec), Some(NodeSpec::Sma { .. })));
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
    fn stable_bars_for_only_counts_applicable_overlays() {
        let overlays = vec![
            Overlay {
                name: "a".to_string(),
                spec: NodeSpec::Sma {
                    source: Box::new(NodeSpec::Close { source: None }),
                    period: nz(200),
                },
                scope: OverlayScope {
                    symbol: Some("BTC".to_string()),
                    interval: None,
                },
                origin: "(test)".to_string(),
            },
            Overlay {
                name: "b".to_string(),
                spec: NodeSpec::Sma {
                    source: Box::new(NodeSpec::Close { source: None }),
                    period: nz(20),
                },
                scope: OverlayScope::default(),
                origin: "(test)".to_string(),
            },
        ];
        let cols = column_names(&overlays);
        assert_eq!(stable_bars_for(&overlays, &cols, "BTC", Interval::Day(1), &Schema::empty()).unwrap(), 200);
        assert_eq!(stable_bars_for(&overlays, &cols, "ETH", Interval::Day(1), &Schema::empty()).unwrap(), 20);
    }

    #[test]
    fn stable_bars_uses_active_override_not_the_shadowed_global() {
        // Global `ma=SMA(200)` shadowed for BTC by `ma=SMA(30)`. BTC's warm-up
        // must reflect the BTC override (30), not the shadowed 200.
        let a = Source::Inline("ma=!sma { period: 200 }".to_string());
        let b = Source::Inline("BTC:ma=!sma { period: 30 }".to_string());
        let overlays = parse_specs(&[a, b]).unwrap();
        let cols = column_names(&overlays);
        assert_eq!(stable_bars_for(&overlays, &cols, "BTC", Interval::Day(1), &Schema::empty()).unwrap(), 30);
        assert_eq!(stable_bars_for(&overlays, &cols, "ETH", Interval::Day(1), &Schema::empty()).unwrap(), 200);
    }

    #[test]
    fn stable_bars_derives_from_library() {
        // Sanity check: the value comes straight from Indicator::stable_bars()
        // on the freshly-built DynValue.
        let src = Source::Inline("s=!sma { period: 14 }".to_string());
        let overlays = parse_specs(std::slice::from_ref(&src)).unwrap();
        let cols = column_names(&overlays);
        assert_eq!(stable_bars_for(&overlays, &cols, "X", Interval::Day(1), &Schema::empty()).unwrap(), 14);
    }

    #[test]
    fn param_substitutes_in_inline_overlay() {
        // `!param FAST` inside an inline overlay expression resolves from the
        // `--params` table before the typed `NodeSpec` parse, exactly as it
        // does in a strategy document.
        let src = Source::Inline("ma=!sma { period: !param FAST }".to_string());
        let table = HashMap::from([("FAST".to_string(), Json::from(20))]);
        let overlays = super::parse_specs(std::slice::from_ref(&src), &table).unwrap();
        assert_eq!(overlays.len(), 1);
        assert!(matches!(
            &overlays[0].spec,
            NodeSpec::Sma { period, .. } if period.get() == 20
        ));
    }

    #[test]
    fn param_default_applies_when_unset() {
        let src = Source::Inline("ma=!sma { period: !param { key: FAST, default: 14 } }".to_string());
        let overlays = super::parse_specs(std::slice::from_ref(&src), &HashMap::new()).unwrap();
        assert!(matches!(
            &overlays[0].spec,
            NodeSpec::Sma { period, .. } if period.get() == 14
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
            &overlays[0].spec,
            NodeSpec::Sma { period, .. } if period.get() == 30
        ));
    }
}
