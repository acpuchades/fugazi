//! The `--series` long-dataframe loader.
//!
//! Each `--series` flag describes one table as a `,`-separated list of terms:
//!
//! * `key=value` — a **literal** column, the constant `value` broadcast across
//!   every row of the series;
//! * `@path` — a **CSV file** whose header columns and rows become the series'
//!   columns and rows (several `@files` in one series concatenate their rows).
//!   Each file's column delimiter is autodetected from its header.
//!
//! Within a series the literals are merged onto every loaded row (a literal wins
//! a name clash). Across all `--series` flags the resulting tables are
//! **full-outer-joined on `(symbol, freq, time)`** into one long dataframe: a
//! `BTreeMap` keyed by that triple, so iteration is ascending by symbol, then by
//! cadence, then by `time` — and `time` is compared as the opaque,
//! caller-sorted string it was given (dates, epochs, anything).
//!
//! # Why `freq` is part of the key
//!
//! It used to not be, and the join was on `(symbol, time)` alone. `fugazi get
//! binance:BTCUSDT[1d,1h]` writes both cadences into one file, both stamped
//! RFC 3339, so the daily bar and the midnight hourly bar carried the *same*
//! `time` — and merged into one row, last writer winning the OHLCV. The other 23
//! hourly bars survived alongside, cadence detection then read a ~1h median off
//! the wreckage, and every visible surface — row count, date range, symbol list —
//! still looked right. Keying on the cadence keeps the two series apart; what to
//! *do* about a symbol that carries two of them is [`crate::cadence`]'s job.
//!
//! **An absent or empty `freq` cell is not a cadence, it is a missing label.**
//! Keying it as its own group would break the documented two-`get`-into-two-`-s`
//! join the moment one side is a hand-written overlay CSV with no `freq` column.
//! So a row with no cadence adopts its symbol's **sole declared** cadence, if the
//! symbol declares exactly one across the whole load. That inference happens in a
//! pass *before* any insert, so the "later `--series` wins a column clash" rule is
//! preserved exactly: the rows still merge in the order they were given. When a
//! symbol declares two or more cadences the untagged rows stay in their own `""`
//! group, where the cadence census reports them rather than guessing.

use std::collections::HashMap;
use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;
use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result, anyhow, bail};
use fugazi::prelude::*;

use crate::calendar;

/// A column-keyed row; column names are lowercased for case-insensitive lookup.
type Row = HashMap<String, String>;

/// Columns treated as OHLCV or metadata and therefore never lifted into an
/// overlay schema. Everything else in a row is a candidate overlay column.
const RESERVED_COLUMNS: &[&str] = &[
    "symbol", "time", "freq", "open", "high", "low", "close", "volume",
];

/// Classification state for one candidate overlay column, accumulated across
/// a symbol's rows. Two flags start `true` and monotonically flip to `false`
/// on the first observation that violates them; after the pass the type is
/// picked in priority order **Bool > Real > Str** (both `_ok` → Bool, only
/// `real_ok` → Real, otherwise → Str).
///
/// This is what lets a `true`/`false` column drop straight into a `!get`
/// signal position without a `!str_eq` gymnastics — CSV makes those tokens
/// unambiguously not-numeric-and-not-general-strings.
#[derive(Debug, Clone, Copy)]
struct ColumnState {
    /// Every non-empty value observed so far is a case-insensitive
    /// `true`/`false`.
    bool_ok: bool,
    /// Every non-empty value observed so far parses as [`Real`].
    real_ok: bool,
    /// Whether any non-empty value has been observed. An all-empty column has
    /// no evidence for either type; we register it as `Str` (harmless — the
    /// atoms will carry empty strings, which read as `""` back out).
    seen_any: bool,
}

impl ColumnState {
    fn new() -> Self {
        Self {
            bool_ok: true,
            real_ok: true,
            seen_any: false,
        }
    }

    fn observe(&mut self, value: &str) {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return; // missing carries no signal about the column type
        }
        self.seen_any = true;
        if !is_bool_token(trimmed) {
            self.bool_ok = false;
        }
        if trimmed.parse::<Real>().is_err() {
            self.real_ok = false;
        }
    }

    /// Resolve to the declared [`OverlayType`] after all rows are observed.
    fn resolve(&self) -> OverlayType {
        if self.seen_any && self.bool_ok {
            OverlayType::Bool
        } else if self.seen_any && self.real_ok {
            OverlayType::Real
        } else {
            OverlayType::Str
        }
    }
}

/// Case-insensitive `true` / `false` recognizer. Deliberately narrow: no
/// `yes`/`no`/`1`/`0` — those overlap with `Real` and would break the
/// priority ordering.
fn is_bool_token(s: &str) -> bool {
    s.eq_ignore_ascii_case("true") || s.eq_ignore_ascii_case("false")
}

/// Parse a `true`/`false` token to bool. Caller must have already accepted it
/// via [`is_bool_token`]; anything else is a defensive `false`.
fn parse_bool_token(s: &str) -> bool {
    s.eq_ignore_ascii_case("true")
}

/// The atom series for one symbol: per-bar `(time, atom)` pairs plus a
/// vestigial "skipped columns" list (always empty in the current
/// implementation — retained so the existing warning banners in
/// [`crate::run`] / [`crate::optimize`] compile unchanged; follow-up cleanup
/// will remove both the field and the banners).
#[derive(Debug)]
pub struct AtomSeries {
    /// One `(time, atom)` per bar, ascending by `time`.
    pub atoms: Vec<(String, Atom)>,
    /// **Deprecated.** Non-reserved columns that used to be dropped from the
    /// schema for carrying a non-numeric value; the loader now preserves
    /// those as `Str` overlays and returns an empty list here.
    pub skipped_columns: Vec<String>,
}

/// One `--series` argument, parsed into its `key=value` literal columns and
/// `@file` CSV loaders. (Clap parses each `--series` value through [`FromStr`].)
#[derive(Debug, Clone)]
pub struct SeriesSpec {
    /// The raw flag value, kept for error messages.
    raw: String,
    /// Constant columns broadcast across every loaded row (lowercased keys).
    literals: Vec<(String, String)>,
    /// CSV files whose rows are concatenated.
    files: Vec<String>,
}

impl FromStr for SeriesSpec {
    type Err = String;

    fn from_str(spec: &str) -> Result<Self, Self::Err> {
        let mut literals = Vec::new();
        let mut files = Vec::new();
        for term in spec.split(',') {
            let term = term.trim();
            if term.is_empty() {
                continue;
            }
            if let Some(path) = term.strip_prefix('@') {
                files.push(path.to_string());
            } else if let Some((key, value)) = term.split_once('=') {
                let value = unquote(value.trim());
                // A literal value should never contain '@' — that means an `@file`
                // term got swallowed, usually because terms were joined with ';'
                // (the CSV delimiter) instead of ','.
                if value.contains('@') {
                    return Err(format!(
                        "series term `{term}`: a literal value can't contain '@'. Series terms \
                         are separated by ',' — e.g. \"symbol=AAPL,@candles.csv\""
                    ));
                }
                literals.push((key.trim().to_lowercase(), value.to_string()));
            } else {
                return Err(format!(
                    "series term `{term}` is neither a `key=value` literal nor an `@file`"
                ));
            }
        }
        // Every series must load at least one CSV; literals only make sense
        // broadcast over a file's rows (and a literals-only row has no `time`).
        if files.is_empty() {
            return Err(format!(
                "series `{spec}` loads no CSV: every series needs at least one `@file.csv` term \
                 (terms are separated by ',')"
            ));
        }
        Ok(SeriesSpec {
            raw: spec.to_string(),
            literals,
            files,
        })
    }
}

impl SeriesSpec {
    /// Load this series' rows: each file's rows, with the literals broadcast onto
    /// every one (a literal wins a name clash).
    fn rows(&self) -> Result<Vec<Row>> {
        let mut rows = Vec::new();
        for path in &self.files {
            for mut row in read_csv(path)? {
                row.extend(self.literals.iter().map(|(k, v)| (k.clone(), v.clone())));
                rows.push(row);
            }
        }
        Ok(rows)
    }
}

/// The merged long dataframe: rows keyed by `(symbol, freq, time)`.
///
/// The middle component is the `freq` **cell as written**, trimmed but never
/// case-folded — `1M` is a month and `1m` is a minute, so lowercasing the key
/// would silently fuse them. `""` means the row carried no cadence label and
/// none could be inferred (see the module docs).
#[derive(Debug, Default)]
pub struct DataFrame {
    rows: BTreeMap<(String, String, String), Row>,
    /// Memoized frame-wide overlay schema — see
    /// [`shared_schema`](Self::shared_schema). Every atom the frame produces
    /// binds to this one `Arc`, which is what makes a cross-symbol `!get`
    /// resolve.
    schema: OnceLock<Option<Arc<Schema>>>,
}

impl DataFrame {
    /// Build the dataframe from the parsed `--series` specs. Each `@file`'s column
    /// delimiter is autodetected from its header.
    pub fn from_series(series: &[SeriesSpec]) -> Result<Self> {
        // Every row is materialised before the first insert, because the
        // cadence an *untagged* row belongs to is a property of the whole load
        // (its symbol's sole declared cadence, if there is exactly one) and is
        // not knowable while streaming. The frame holds them all a moment later
        // anyway, so this costs ordering, not peak memory — and keeping the
        // insert order intact is what preserves "the later `--series` wins".
        let mut loaded: Vec<(&str, Row)> = Vec::new();
        for spec in series {
            for row in spec.rows()? {
                loaded.push((spec.raw.as_str(), row));
            }
        }
        let fallback = sole_declared_cadences(&loaded);

        let mut frame = DataFrame::default();
        for (raw, row) in loaded {
            frame.insert(raw, row, &fallback)?;
        }
        Ok(frame)
    }

    /// Every unique `symbol` present in the frame, in ascending order.
    /// Consumed by the basket driver to discover the tradeable universe
    /// without an explicit up-front declaration.
    pub fn symbols(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .rows
            .keys()
            .map(|(sym, _, _)| sym.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        out.sort();
        out
    }

    /// The distinct `freq` cells carried by `symbol`, ascending, `""` for the
    /// untagged group. Empty when the frame holds no rows for it.
    ///
    /// More than one entry means the frame carries two series under one name
    /// and nothing here can choose between them — [`crate::cadence`] resolves
    /// that against `-f/--frequency` before anything reads the frame.
    pub fn frequencies_of(&self, symbol: &str) -> Vec<&str> {
        // The keys are sorted, so a symbol's cadences are contiguous and
        // `dedup` is enough — no set needed.
        let mut out: Vec<&str> = self
            .rows
            .keys()
            .filter(|(sym, _, _)| sym == symbol)
            .map(|(_, freq, _)| freq.as_str())
            .collect();
        out.dedup();
        out
    }

    /// The cadence `symbol`'s rows **declare**, parsed — authoritative over
    /// anything detected from timestamp gaps, because it is what the provider
    /// (or the user's `freq=` literal) said the bars are.
    ///
    /// `None` when the symbol is untagged, carries an unparseable label, or
    /// carries more than one cadence. The last case is only reachable before
    /// [`crate::cadence`] has resolved the frame; after it, a symbol has at
    /// most one.
    pub fn declared_frequency(&self, symbol: &str) -> Option<Frequency> {
        match self.frequencies_of(symbol).as_slice() {
            [only] if !only.is_empty() => Frequency::from_str(only).ok(),
            _ => None,
        }
    }

    /// Every `(symbol, freq cell, sorted bar stamps)` group in the frame — the
    /// census's one read of the loaded data.
    ///
    /// Stamps are the parsed `time` column, ascending **numerically**. The key
    /// order is ascending by the time *string*, which is the same thing for
    /// RFC 3339 and for fixed-width dates but not for bare epoch integers, and
    /// a cadence read off gaps between out-of-order stamps is noise. Rows whose
    /// `time` matches no known shape contribute nothing — they cannot be spaced
    /// against anything.
    pub fn cadence_groups(&self) -> Vec<(String, String, Vec<i64>)> {
        let mut by_group: BTreeMap<(&str, &str), Vec<i64>> = BTreeMap::new();
        for (sym, freq, time) in self.rows.keys() {
            let bucket = by_group.entry((sym.as_str(), freq.as_str())).or_default();
            if let Some(ms) = calendar::parse_time_to_millis(time) {
                bucket.push(ms);
            }
        }
        by_group
            .into_iter()
            .map(|((sym, freq), mut stamps)| {
                stamps.sort_unstable();
                (sym.to_string(), freq.to_string(), stamps)
            })
            .collect()
    }

    /// Drop every row of `symbol` that is not in the `freq` cadence group.
    ///
    /// The disambiguation half of the census: once `-f/--frequency` has named
    /// which of a symbol's cadences the run targets, the others are pruned here
    /// so nothing downstream has to carry the choice. A symbol the frame does
    /// not hold, or a cadence it does not have, prunes to nothing rather than
    /// erroring — the caller checked both before asking.
    pub fn retain_cadence(&mut self, symbol: &str, freq: &str) {
        // Pruning can retire the last row carrying some column, so the memoized
        // schema is stale for the same reason `insert` invalidates it.
        self.schema.take();
        self.rows
            .retain(|(sym, f, _), _| sym != symbol || f == freq);
    }

    /// Merge one row into the frame, joining on `(symbol, freq, time)`.
    ///
    /// `untagged_fallback` maps a symbol to the sole cadence it declares
    /// elsewhere in this load; a row with no `freq` cell of its own joins that
    /// group. See the module docs for why that inference exists.
    fn insert(
        &mut self,
        spec: &str,
        row: Row,
        untagged_fallback: &HashMap<String, String>,
    ) -> Result<()> {
        // A new row can introduce a column, so any schema built from the
        // previous contents is stale. In practice every insert happens inside
        // `from_series` before the first `atoms` call, but nothing in the type
        // enforces that ordering.
        self.schema.take();
        let symbol = row
            .get("symbol")
            .cloned()
            .ok_or_else(|| anyhow!("series `{spec}`: a row is missing a `symbol` column"))?;
        let time = row
            .get("time")
            .cloned()
            .ok_or_else(|| anyhow!("series `{spec}`: a row is missing a `time` column"))?;
        let freq = declared_cadence(&row)
            .map(str::to_string)
            .or_else(|| untagged_fallback.get(&symbol).cloned())
            .unwrap_or_default();
        self.rows
            .entry((symbol, freq, time))
            .or_default()
            .extend(row);
        Ok(())
    }

    /// The one overlay [`Schema`] every atom in the frame binds to, built from
    /// every non-reserved column across **all** symbols and memoized so each
    /// call hands back the same `Arc`.
    ///
    /// Frame-wide rather than per-symbol on purpose. A strategy resolves
    /// `!get { key }` once, against the run's schema, and
    /// [`GetReal`](crate::indicators::GetReal) guards every read with
    /// `Arc::ptr_eq` against the schema the atom is bound to. Build a schema
    /// per symbol and a cross-symbol read — `!get` through
    /// `!pick { symbol }` — sees a different `Arc` and returns `None` on every
    /// bar, silently. One schema for the frame makes the pointers match and
    /// registers a column for every symbol, including the ones whose rows never
    /// carried it: those read as an absent sample, which is what they are.
    ///
    /// `None` when no symbol has any non-reserved column, i.e. there is no
    /// side channel at all.
    fn shared_schema(&self) -> Option<&Arc<Schema>> {
        self.schema
            .get_or_init(|| {
                let mut classification: BTreeMap<String, ColumnState> = BTreeMap::new();
                for row in self.rows.values() {
                    for (name, value) in row {
                        if RESERVED_COLUMNS.contains(&name.as_str()) {
                            continue;
                        }
                        classification
                            .entry(name.clone())
                            .or_insert_with(ColumnState::new)
                            .observe(value);
                    }
                }
                if classification.is_empty() {
                    return None;
                }
                // BTreeMap iterates alphabetically, so the column order — and
                // therefore every index `Get` resolves — is deterministic.
                let mut b = Schema::builder();
                for (name, state) in &classification {
                    match state.resolve() {
                        OverlayType::Real => b.add_real(name.clone()),
                        OverlayType::Bool => b.add_bool(name.clone()),
                        OverlayType::Str => b.add_str(name.clone()),
                    };
                }
                Some(b.finish())
            })
            .as_ref()
    }

    /// The atom series for `symbol`, ascending by `time`: OHLCV candles plus
    /// per-bar overlay values keyed by the frame-wide [`Schema`] every symbol
    /// shares (see [`shared_schema`](Self::shared_schema)).
    ///
    /// Each candidate overlay column is auto-classified across its observed
    /// values by the **Bool > Real > Str** priority classifier
    /// ([`ColumnState`]) — every column survives, whatever its cell shape.
    /// A missing cell reads as `Real::NAN` for a `Real` column, `false` for a
    /// `Bool` column, and `""` for a `Str` column. Schema columns are ordered
    /// alphabetically for determinism.
    pub fn atoms(&self, symbol: &str) -> Result<AtomSeries> {
        // Two cadences under one symbol are two series, and interleaving them
        // into one atom stream would produce a bar order that is neither. The
        // caller is expected to have resolved the frame through
        // `crate::cadence` first; this is the guard for every path that did
        // not, and it refuses rather than picking.
        match self.frequencies_of(symbol).as_slice() {
            [] => bail!("no rows found for symbol `{symbol}` across the given --series"),
            [_] => {}
            many => bail!(
                "symbol `{symbol}` carries {} cadences in the input series ({}) — \
                 pass `-f/--frequency {symbol}:<CODE>` to say which one to trade",
                many.len(),
                many.iter()
                    .map(|f| if f.is_empty() { "<untagged>" } else { *f })
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
        }

        let schema = self.shared_schema();
        // Read the column order back off the schema rather than keeping a
        // parallel list, so the cells can never drift from the indexes `Get`
        // resolved against it.
        let column_types: Vec<(String, OverlayType)> = schema
            .map(|s| {
                s.keys()
                    .map(|k| {
                        (
                            k.to_string(),
                            s.type_of_key(k).expect("key came from keys()"),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();

        // Second pass: build one atom per row, attaching overlays when the
        // schema has any columns.
        let mut atoms = Vec::new();
        for ((sym, _freq, time), row) in &self.rows {
            if sym != symbol {
                continue;
            }
            let candle = row_to_candle(sym, time, row)?;
            // Parse the row's `time` label into a UTC-ms `Timestamp` when it
            // matches a known shape (RFC3339, `YYYY-MM-DD [HH:MM:SS]`, epoch
            // s/ms) — so `Atom::time` carries the real bar-open time and the
            // calendar indicators / duration-form `-w` don't have to re-parse.
            // An unparseable label leaves `time` as `None`; the strings still
            // sort the frame the way the user typed them.
            let ts = calendar::parse_time_to_millis(time).map(Timestamp);
            // Built field-wise rather than through the constructors: with the
            // candle optional there are eight combinations, and naming them all
            // reads worse than the three fields do.
            let overlays = schema.map(|schema| {
                let values: Vec<OverlayValue> = column_types
                    .iter()
                    .map(|(name, ty)| {
                        let raw = row.get(name).map(|s| s.trim()).unwrap_or("");
                        cell_to_overlay(raw, *ty)
                    })
                    .collect();
                OverlayInfo::new(schema.clone(), values)
            });
            atoms.push((
                time.clone(),
                Atom {
                    candle,
                    time: ts,
                    overlays,
                },
            ));
        }

        Ok(AtomSeries {
            atoms,
            skipped_columns: Vec::new(),
        })
    }
}

/// A row's own cadence label: the `freq` cell, trimmed, or `None` when the
/// column is absent or the cell is blank.
///
/// Not case-folded — `Frequency` spells month `1M` and minute `1m`, so the
/// case *is* the unit.
fn declared_cadence(row: &Row) -> Option<&str> {
    row.get("freq").map(|s| s.trim()).filter(|s| !s.is_empty())
}

/// Per symbol, the one cadence it declares — present only for symbols that
/// declare exactly one across the whole load.
///
/// This is what an untagged row adopts. A symbol declaring two cadences is
/// absent from the map on purpose: there is no sole cadence to adopt, and
/// picking one would hide the ambiguity the census exists to report.
fn sole_declared_cadences(loaded: &[(&str, Row)]) -> HashMap<String, String> {
    let mut seen: HashMap<&str, BTreeSet<&str>> = HashMap::new();
    for (_, row) in loaded {
        // A row with no `symbol` is an error, but `insert` is where it is
        // reported; skipping it here keeps that diagnostic the one the user
        // sees.
        let (Some(symbol), Some(freq)) = (row.get("symbol"), declared_cadence(row)) else {
            continue;
        };
        seen.entry(symbol.as_str()).or_default().insert(freq);
    }
    seen.into_iter()
        .filter_map(
            |(sym, freqs)| match freqs.into_iter().collect::<Vec<_>>().as_slice() {
                [only] => Some((sym.to_string(), only.to_string())),
                _ => None,
            },
        )
        .collect()
}

/// Convert a raw CSV cell to an [`OverlayValue`] of the declared type.
/// Missing / empty cells fall through to type-appropriate defaults
/// (`Real::NAN`, `false`, `""`) — matching the pre-widening behaviour of the
/// old `Real::NAN` fallback for numeric columns.
fn cell_to_overlay(raw: &str, ty: OverlayType) -> OverlayValue {
    match ty {
        OverlayType::Real => {
            if raw.is_empty() {
                OverlayValue::Real(Real::NAN)
            } else {
                OverlayValue::Real(raw.parse::<Real>().unwrap_or(Real::NAN))
            }
        }
        OverlayType::Bool => {
            if raw.is_empty() {
                OverlayValue::Bool(false)
            } else {
                OverlayValue::Bool(parse_bool_token(raw))
            }
        }
        OverlayType::Str => OverlayValue::Str(std::sync::Arc::from(raw)),
    }
}

/// Build a [`Candle`] from one row's OHLCV columns, or `None` when the row
/// carries no price at all — an **overlay-only** series such as a funding rate
/// or an open interest, which is stacked into the run beside the price series
/// and read with `!pick` + `!get`.
///
/// A column counts as present only when it is also non-empty, because
/// `fugazi get` writes a blank OHLCV block for overlay rows: the header is
/// there, the cells are not. Testing for the key alone would make that file
/// fail to load back through `--series`.
///
/// A row carrying *some* of the four is an error rather than a third case. A
/// half-filled price bar is a malformed candle, not a series that isn't a
/// price, and silently demoting it to overlay-only would hide the typo.
fn row_to_candle(sym: &str, time: &str, row: &Row) -> Result<Option<Candle>> {
    const OHLC: [&str; 4] = ["open", "high", "low", "close"];
    let filled = |name: &str| row.get(name).is_some_and(|v| !v.trim().is_empty());
    let present = OHLC.iter().filter(|n| filled(n)).count();
    if present == 0 {
        return Ok(None);
    }
    if present < OHLC.len() {
        let missing: Vec<&str> = OHLC.iter().copied().filter(|n| !filled(n)).collect();
        bail!(
            "{sym} @ {time}: price bar is missing `{}` — a row with some OHLC columns \
             but not all is a malformed candle, not an overlay-only series",
            missing.join("`, `")
        );
    }
    // `"nan"`, `"NaN"`, `"inf"` and `"-inf"` all parse as `f64`, and a price has
    // no `None` to fall back to the way an overlay cell does — a `Candle` is
    // five bare `Real`s. Left alone, one such cell poisoned the whole run
    // silently: the position marked at a `NaN`, equity went `NaN`, and the
    // report printed `return NaN% ann` beside a plausible fill list. An
    // unparseable number is already a row error here; a non-finite one is the
    // same kind of bad data. A genuine gap is a row that is *absent*, not a row
    // that says `NaN`.
    let finite = |x: Real, name: &str, raw: &str| -> Result<Real> {
        if x.is_finite() {
            Ok(x)
        } else {
            bail!(
                "{sym} @ {time}: column `{name}` = {raw:?} is not a finite \
                 number — a price has no \"absent\" value; omit the row instead"
            )
        }
    };
    let field = |name: &str| -> Result<Real> {
        let raw = row
            .get(name)
            .ok_or_else(|| anyhow!("{sym} @ {time}: missing required column `{name}`"))?;
        let x = raw
            .parse::<Real>()
            .with_context(|| format!("{sym} @ {time}: column `{name}` = {raw:?}"))?;
        finite(x, name, raw)
    };
    let volume = match row.get("volume") {
        Some(raw) if !raw.is_empty() => {
            let v = raw
                .parse::<Real>()
                .with_context(|| format!("{sym} @ {time}: column `volume` = {raw:?}"))?;
            finite(v, "volume", raw)?
        }
        _ => 0.0,
    };
    Ok(Some(Candle::new(
        field("open")?,
        field("high")?,
        field("low")?,
        field("close")?,
        volume,
    )))
}

/// Read a CSV file into lowercased-column rows, autodetecting its delimiter.
fn read_csv(path: &str) -> Result<Vec<Row>> {
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(crate::csv_source::detect_delimiter(std::path::Path::new(
            path,
        ))?)
        .from_path(path)
        .with_context(|| format!("opening CSV `{path}`"))?;
    let headers: Vec<String> = reader
        .headers()
        .with_context(|| format!("reading header of `{path}`"))?
        .iter()
        .map(|h| h.trim().to_lowercase())
        .collect();
    let mut rows = Vec::new();
    for record in reader.records() {
        let record = record.with_context(|| format!("reading a row of `{path}`"))?;
        let row: Row = headers
            .iter()
            .cloned()
            .zip(record.iter().map(|v| v.trim().to_string()))
            .collect();
        rows.push(row);
    }
    Ok(rows)
}

/// Strip a single matching pair of surrounding quotes (shells pass `'BTC'`
/// through inside a quoted `--series`).
fn unquote(value: &str) -> &str {
    let bytes = value.as_bytes();
    if value.len() >= 2
        && (bytes[0] == b'\'' || bytes[0] == b'"')
        && bytes[bytes.len() - 1] == bytes[0]
    {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::Arc;

    /// Write `contents` to a scratch CSV and hand back its path.
    ///
    /// The name is salted with the pid and a counter rather than used
    /// verbatim: `/tmp` is shared, so a fixed name collides with a concurrent
    /// `cargo test`, with another checkout, and with another user's leftovers —
    /// and the failure reads as a data bug rather than as the clash it is.
    /// Same reasoning as `tests/common/cli.rs::unique_path`.
    fn tmp_csv(name: &str, contents: &str) -> String {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let unique = format!(
            "{}_{}_{name}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        );
        let path = std::env::temp_dir().join(unique);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn literal_stamps_symbol_onto_a_symbolless_file() {
        let path = tmp_csv(
            "fugazi_data_test_a.csv",
            "time;open;high;low;close;volume\n1;10;11;9;10.5;100\n2;10.5;12;10;11;120\n",
        );
        let frame =
            DataFrame::from_series(&[format!("symbol='BTC',@{path}").parse().unwrap()]).unwrap();
        let series = frame.atoms("BTC").unwrap();
        assert_eq!(series.atoms.len(), 2);
        assert_eq!(series.atoms[0].0, "1");
        assert_eq!(series.atoms[0].1.candle.unwrap().close, 10.5);
        assert_eq!(series.atoms[1].1.candle.unwrap().high, 12.0);
    }

    #[test]
    fn two_series_full_join_on_symbol_freq_time() {
        let prices = tmp_csv(
            "fugazi_data_test_p.csv",
            "time;open;high;low;close\n1;10;11;9;10\n2;10;12;10;11\n",
        );
        let fundamentals = tmp_csv("fugazi_data_test_f.csv", "time;pe_ratio\n1;15.0\n2;16.0\n");
        let frame = DataFrame::from_series(&[
            format!("symbol=BTC,@{prices}").parse().unwrap(),
            format!("symbol=BTC,@{fundamentals}").parse().unwrap(),
        ])
        .unwrap();
        // The extra column rode along on the joined rows.
        assert_eq!(
            frame.rows[&("BTC".into(), String::new(), "1".into())]["pe_ratio"],
            "15.0"
        );
        // Candles still build (volume defaulted to 0).
        let series = frame.atoms("BTC").unwrap();
        assert_eq!(series.atoms.len(), 2);
        assert_eq!(series.atoms[0].1.candle.unwrap().volume, 0.0);
    }

    #[test]
    fn files_and_literals_in_any_order_and_count() {
        let p1 = tmp_csv(
            "fugazi_data_test_o1.csv",
            "time;open;high;low;close\n1;10;11;9;10\n2;10;12;10;11\n",
        );
        let p2 = tmp_csv(
            "fugazi_data_test_o2.csv",
            "time;open;high;low;close\n3;11;13;11;12\n4;12;14;12;13\n",
        );
        // Mixed order, two files and two literals in one series.
        let frame = DataFrame::from_series(&[format!("symbol=BTC,@{p1},exchange=NYSE,@{p2}")
            .parse()
            .unwrap()])
        .unwrap();
        // Both files' rows concatenated.
        assert_eq!(frame.atoms("BTC").unwrap().atoms.len(), 4);
        // Both literals broadcast onto rows from either file.
        assert_eq!(
            frame.rows[&("BTC".into(), String::new(), "1".into())]["exchange"],
            "NYSE"
        );
        assert_eq!(
            frame.rows[&("BTC".into(), String::new(), "4".into())]["exchange"],
            "NYSE"
        );
    }

    #[test]
    fn series_without_a_file_is_rejected() {
        assert!("symbol=BTC".parse::<SeriesSpec>().is_err());
    }

    #[test]
    fn atoms_expose_extra_numeric_columns_as_overlays() {
        let path = tmp_csv(
            "fugazi_atoms_numeric.csv",
            "time;open;high;low;close;vol_20;regime_score\n\
             1;10;11;9;10;0.12;1.0\n\
             2;10;12;10;11;0.15;0.5\n",
        );
        let frame =
            DataFrame::from_series(&[format!("symbol=BTC,@{path}").parse().unwrap()]).unwrap();
        let series = frame.atoms("BTC").unwrap();
        assert_eq!(series.atoms.len(), 2);
        assert!(series.skipped_columns.is_empty());
        let (_, atom0) = &series.atoms[0];
        let overlays = atom0.overlays.as_ref().expect("first bar carries overlays");
        let schema = overlays.schema();
        // Alphabetical order: regime_score, vol_20.
        assert_eq!(schema.index_of("regime_score"), Some(0));
        assert_eq!(schema.index_of("vol_20"), Some(1));
        assert_eq!(
            overlays.get_real(schema.index_of("vol_20").unwrap()),
            Some(0.12)
        );
        assert_eq!(
            overlays.get_real(schema.index_of("regime_score").unwrap()),
            Some(1.0)
        );
    }

    #[test]
    fn atoms_preserve_non_numeric_columns_as_str_overlays() {
        let path = tmp_csv(
            "fugazi_atoms_nonnumeric.csv",
            "time;open;high;low;close;exchange;vol_20\n\
             1;10;11;9;10;NYSE;0.12\n\
             2;10;12;10;11;NASDAQ;0.15\n",
        );
        let frame =
            DataFrame::from_series(&[format!("symbol=BTC,@{path}").parse().unwrap()]).unwrap();
        let series = frame.atoms("BTC").unwrap();
        // `exchange` is non-numeric — preserved as a Str overlay, not dropped.
        assert!(series.skipped_columns.is_empty());
        let overlays = series.atoms[0]
            .1
            .overlays
            .as_ref()
            .expect("overlays attached");
        let schema = overlays.schema();
        assert_eq!(schema.type_of_key("exchange"), Some(OverlayType::Str));
        assert_eq!(schema.type_of_key("vol_20"), Some(OverlayType::Real));
        let ex_idx = schema.index_of("exchange").unwrap();
        assert_eq!(overlays.get_str(ex_idx).map(|s| s.as_ref()), Some("NYSE"),);
        assert_eq!(
            series.atoms[1]
                .1
                .overlays
                .as_ref()
                .unwrap()
                .get_str(ex_idx)
                .map(|s| s.as_ref()),
            Some("NASDAQ"),
        );
    }

    #[test]
    fn atoms_classify_true_false_column_as_bool() {
        let path = tmp_csv(
            "fugazi_atoms_bool.csv",
            "time;open;high;low;close;risk_on\n\
             1;10;11;9;10;true\n\
             2;10;12;10;11;FALSE\n\
             3;10;12;10;11;True\n",
        );
        let frame =
            DataFrame::from_series(&[format!("symbol=BTC,@{path}").parse().unwrap()]).unwrap();
        let series = frame.atoms("BTC").unwrap();
        let overlays = series.atoms[0]
            .1
            .overlays
            .as_ref()
            .expect("overlays attached");
        let schema = overlays.schema();
        // Bool wins over Real because `true`/`false` don't parse as Real.
        assert_eq!(schema.type_of_key("risk_on"), Some(OverlayType::Bool));
        let idx = schema.index_of("risk_on").unwrap();
        assert_eq!(overlays.get_bool(idx), Some(true));
        assert_eq!(
            series.atoms[1].1.overlays.as_ref().unwrap().get_bool(idx),
            Some(false),
        );
        assert_eq!(
            series.atoms[2].1.overlays.as_ref().unwrap().get_bool(idx),
            Some(true),
        );
    }

    #[test]
    fn atoms_single_stray_string_downgrades_a_real_column_to_str() {
        // One non-numeric cell is enough to move a column from Real to Str;
        // subsequent numeric-looking cells then read as their string form.
        let path = tmp_csv(
            "fugazi_atoms_mixed.csv",
            "time;open;high;low;close;label\n\
             1;10;11;9;10;0.12\n\
             2;10;12;10;11;n/a\n\
             3;10;12;10;11;0.15\n",
        );
        let frame =
            DataFrame::from_series(&[format!("symbol=BTC,@{path}").parse().unwrap()]).unwrap();
        let series = frame.atoms("BTC").unwrap();
        let schema = series.atoms[0]
            .1
            .overlays
            .as_ref()
            .unwrap()
            .schema()
            .clone();
        assert_eq!(schema.type_of_key("label"), Some(OverlayType::Str));
        let idx = schema.index_of("label").unwrap();
        assert_eq!(
            series.atoms[2]
                .1
                .overlays
                .as_ref()
                .unwrap()
                .get_str(idx)
                .map(|s| s.as_ref()),
            Some("0.15"),
        );
    }

    #[test]
    fn atoms_use_nan_for_missing_overlay_cells() {
        let prices = tmp_csv(
            "fugazi_atoms_prices.csv",
            "time;open;high;low;close\n1;10;11;9;10\n2;10;12;10;11\n",
        );
        // Sparse extra column: only present at time=1, missing at time=2.
        let overlay = tmp_csv("fugazi_atoms_overlay.csv", "time;pe_ratio\n1;15.0\n");
        let frame = DataFrame::from_series(&[
            format!("symbol=BTC,@{prices}").parse().unwrap(),
            format!("symbol=BTC,@{overlay}").parse().unwrap(),
        ])
        .unwrap();
        let series = frame.atoms("BTC").unwrap();
        assert_eq!(series.atoms.len(), 2);
        let overlays0 = series.atoms[0].1.overlays.as_ref().unwrap();
        let idx = overlays0.schema().index_of("pe_ratio").unwrap();

        let v0 = overlays0.get_real(idx).unwrap();
        let v1 = series.atoms[1]
            .1
            .overlays
            .as_ref()
            .unwrap()
            .get_real(idx)
            .unwrap();
        assert_eq!(v0, 15.0);
        assert!(v1.is_nan(), "missing overlay value should be NaN, got {v1}");
    }

    #[test]
    fn atoms_attach_str_overlay_even_when_no_numeric_column_survives() {
        // Only OHLCV + a non-numeric metadata column. In the new world the
        // metadata column *is* preserved as a Str overlay — an OverlayInfo
        // gets attached rather than being suppressed.
        let path = tmp_csv(
            "fugazi_atoms_str_only.csv",
            "time;open;high;low;close;exchange\n1;10;11;9;10;NYSE\n",
        );
        let frame =
            DataFrame::from_series(&[format!("symbol=BTC,@{path}").parse().unwrap()]).unwrap();
        let series = frame.atoms("BTC").unwrap();
        assert!(series.skipped_columns.is_empty());
        let overlays = series.atoms[0]
            .1
            .overlays
            .as_ref()
            .expect("Str overlay attached");
        assert_eq!(
            overlays.schema().type_of_key("exchange"),
            Some(OverlayType::Str)
        );
    }

    #[test]
    fn atoms_share_one_schema_across_every_bar() {
        let path = tmp_csv(
            "fugazi_atoms_shared_schema.csv",
            "time;open;high;low;close;vol_20\n\
             1;10;11;9;10;0.1\n\
             2;10;12;10;11;0.2\n\
             3;11;12;10;11;0.3\n",
        );
        let frame =
            DataFrame::from_series(&[format!("symbol=BTC,@{path}").parse().unwrap()]).unwrap();
        let series = frame.atoms("BTC").unwrap();
        let schema0 = series.atoms[0]
            .1
            .overlays
            .as_ref()
            .unwrap()
            .schema()
            .clone();
        for (_, atom) in &series.atoms[1..] {
            let s = atom.overlays.as_ref().unwrap().schema();
            assert!(
                Arc::ptr_eq(&schema0, s),
                "every atom must reuse the shared Arc<Schema>"
            );
        }
    }

    #[test]
    fn atoms_share_one_schema_across_every_symbol() {
        // A column that only one symbol carries must still resolve for the
        // others: `GetReal` is built once against the run's schema and guards
        // reads with `Arc::ptr_eq`, so a per-symbol schema makes a cross-symbol
        // `!get { source: !pick { symbol } }` read `None` on every bar rather
        // than fail loudly.
        let prices = tmp_csv(
            "fugazi_atoms_cross_symbol_prices.csv",
            "symbol;time;open;high;low;close\n\
             BTC;1;10;11;9;10\n\
             ETH;1;20;21;19;20\n",
        );
        let funding = tmp_csv(
            "fugazi_atoms_cross_symbol_funding.csv",
            "symbol;time;funding\n\
             ETH;1;0.5\n",
        );
        let frame = DataFrame::from_series(&[
            format!("@{prices}").parse().unwrap(),
            format!("@{funding}").parse().unwrap(),
        ])
        .unwrap();

        let btc = frame.atoms("BTC").unwrap();
        let eth = frame.atoms("ETH").unwrap();
        let btc_schema = btc.atoms[0].1.overlays.as_ref().unwrap().schema().clone();
        let eth_schema = eth.atoms[0].1.overlays.as_ref().unwrap().schema().clone();

        assert!(
            Arc::ptr_eq(&btc_schema, &eth_schema),
            "every symbol must reuse one Arc<Schema>, or `Get`'s ptr_eq guard rejects the reads"
        );
        assert!(
            btc_schema.contains("funding"),
            "a column only ETH carries must still be registered for BTC"
        );
        // BTC has no cell for it, which reads as an absent sample rather than
        // as a missing column.
        let i = btc_schema.index_of("funding").unwrap();
        assert!(matches!(
            btc.atoms[0].1.overlays.as_ref().unwrap().get(i),
            Some(OverlayValue::Real(v)) if v.is_nan()
        ));
    }

    #[test]
    fn a_series_with_no_ohlc_loads_as_overlay_only_atoms() {
        // A funding series has no price. It must load, carry its column, and
        // stay unpriceable rather than being rejected for missing `open`.
        let path = tmp_csv(
            "fugazi_atoms_overlay_only.csv",
            "symbol;time;funding\nBTC.funding;1;0.0003\nBTC.funding;2;-0.0001\n",
        );
        let frame = DataFrame::from_series(&[format!("@{path}").parse().unwrap()]).unwrap();
        let series = frame.atoms("BTC.funding").unwrap();
        assert_eq!(series.atoms.len(), 2);
        for (_, atom) in &series.atoms {
            assert!(atom.candle.is_none(), "an overlay series carries no bar");
            assert!(!atom.is_priceable());
        }
        let ov = series.atoms[0].1.overlays.as_ref().unwrap();
        let i = ov.schema().index_of("funding").unwrap();
        assert_eq!(ov.get(i), Some(&OverlayValue::Real(0.0003)));
    }

    #[test]
    fn a_blank_ohlcv_block_round_trips_back_through_series() {
        // `fugazi get` writes the header for every column and leaves the OHLCV
        // cells empty on an overlay row. Reading that file back must yield an
        // overlay-only atom, not a parse failure on `""`.
        let path = tmp_csv(
            "fugazi_atoms_blank_ohlcv.csv",
            "symbol;time;open;high;low;close;volume;funding\n\
             BTC.funding;1;;;;;;0.0003\n",
        );
        let frame = DataFrame::from_series(&[format!("@{path}").parse().unwrap()]).unwrap();
        let series = frame.atoms("BTC.funding").unwrap();
        assert!(series.atoms[0].1.candle.is_none());
    }

    #[test]
    fn a_half_filled_price_bar_is_an_error() {
        // Some OHLC but not all is a malformed candle. Demoting it to
        // overlay-only would swallow the typo.
        let path = tmp_csv(
            "fugazi_atoms_half_bar.csv",
            "symbol;time;open;high;close\nBTC;1;10;11;10.5\n",
        );
        let frame = DataFrame::from_series(&[format!("@{path}").parse().unwrap()]).unwrap();
        let err = frame.atoms("BTC").unwrap_err().to_string();
        assert!(err.contains("`low`"), "got {err}");
        assert!(err.contains("malformed candle"), "got {err}");
    }

    #[test]
    fn atoms_reject_unknown_symbol() {
        let path = tmp_csv(
            "fugazi_atoms_unknown_symbol.csv",
            "time;open;high;low;close\n1;10;11;9;10\n",
        );
        let frame =
            DataFrame::from_series(&[format!("symbol=BTC,@{path}").parse().unwrap()]).unwrap();
        assert!(frame.atoms("ETH").is_err());
    }

    // ------------------------------------------------------------- cadences

    /// The bug that put `freq` in the key. `fugazi get SYM[1d,1h]` writes both
    /// cadences to one file, both stamped RFC 3339, so the daily bar and the
    /// midnight hourly bar shared a `time`. Under a `(symbol, time)` key they
    /// merged into one row and one set of OHLCV survived; the other 23 hourly
    /// bars stayed, so the row count, the date range and the symbol list all
    /// still looked right over a series that was neither.
    #[test]
    fn two_cadences_at_one_timestamp_no_longer_collide() {
        let path = tmp_csv(
            "fugazi_cadence_collide.csv",
            "symbol,freq,time,open,high,low,close,volume\n\
             BTC,1d,2024-01-01T00:00:00Z,100,100,100,100,1\n\
             BTC,1h,2024-01-01T00:00:00Z,50,50,50,50,1\n\
             BTC,1h,2024-01-01T01:00:00Z,51,51,51,51,1\n",
        );
        let frame = DataFrame::from_series(&[format!("@{path}").parse().unwrap()]).unwrap();
        // Three rows in, three rows held — the two midnight bars are distinct.
        assert_eq!(frame.rows.len(), 3);
        assert_eq!(
            frame.rows[&("BTC".into(), "1d".into(), "2024-01-01T00:00:00Z".into())]["close"],
            "100"
        );
        assert_eq!(
            frame.rows[&("BTC".into(), "1h".into(), "2024-01-01T00:00:00Z".into())]["close"],
            "50"
        );
        assert_eq!(frame.frequencies_of("BTC"), ["1d", "1h"]);
    }

    /// …and reading it as one stream is refused rather than silently
    /// interleaved. The message has to name both cadences: knowing *which*
    /// stray series leaked in is the whole fix.
    #[test]
    fn atoms_refuse_a_symbol_carrying_two_cadences() {
        let path = tmp_csv(
            "fugazi_cadence_atoms_refuse.csv",
            "symbol,freq,time,open,high,low,close,volume\n\
             BTC,1d,2024-01-01T00:00:00Z,100,100,100,100,1\n\
             BTC,1h,2024-01-01T01:00:00Z,50,50,50,50,1\n",
        );
        let frame = DataFrame::from_series(&[format!("@{path}").parse().unwrap()]).unwrap();
        let err = frame.atoms("BTC").unwrap_err().to_string();
        assert!(err.contains("carries 2 cadences"), "got {err}");
        assert!(err.contains("1d, 1h"), "got {err}");
        assert!(err.contains("-f/--frequency BTC:<CODE>"), "got {err}");
    }

    /// A frame with one cadence per symbol reads exactly as it did before the
    /// key grew a component — including the ascending-by-time guarantee every
    /// caller of `atoms` relies on.
    #[test]
    fn one_cadence_per_symbol_reads_unchanged() {
        let path = tmp_csv(
            "fugazi_cadence_single.csv",
            "symbol,freq,time,open,high,low,close,volume\n\
             BTC,1d,2024-01-02T00:00:00Z,2,2,2,2,1\n\
             BTC,1d,2024-01-01T00:00:00Z,1,1,1,1,1\n\
             ETH,1d,2024-01-01T00:00:00Z,9,9,9,9,1\n",
        );
        let frame = DataFrame::from_series(&[format!("@{path}").parse().unwrap()]).unwrap();
        assert_eq!(frame.symbols(), ["BTC", "ETH"]);
        let series = frame.atoms("BTC").unwrap();
        assert_eq!(series.atoms.len(), 2);
        assert_eq!(series.atoms[0].0, "2024-01-01T00:00:00Z");
        assert_eq!(series.atoms[1].0, "2024-01-02T00:00:00Z");
    }

    /// The documented two-`get`-into-two-`--series` join, with a hand-written
    /// overlay CSV that has no `freq` column. Keying the blank cell as its own
    /// group would put the overlay in a different bucket from the price and
    /// break the join outright.
    #[test]
    fn untagged_rows_join_the_symbols_sole_declared_cadence() {
        let prices = tmp_csv(
            "fugazi_cadence_fold_px.csv",
            "symbol,freq,time,open,high,low,close,volume\n\
             BTC,1d,2024-01-01T00:00:00Z,100,100,100,100,1\n\
             BTC,1d,2024-01-02T00:00:00Z,101,101,101,101,1\n",
        );
        let overlay = tmp_csv(
            "fugazi_cadence_fold_ov.csv",
            "symbol,time,sentiment\n\
             BTC,2024-01-01T00:00:00Z,0.5\n\
             BTC,2024-01-02T00:00:00Z,0.7\n",
        );
        let frame = DataFrame::from_series(&[
            format!("@{prices}").parse().unwrap(),
            format!("@{overlay}").parse().unwrap(),
        ])
        .unwrap();
        assert_eq!(frame.frequencies_of("BTC"), ["1d"]);
        assert_eq!(frame.rows.len(), 2);
        let series = frame.atoms("BTC").unwrap();
        assert_eq!(series.atoms.len(), 2);
        // The overlay landed on the price rows rather than beside them.
        let schema = series.atoms[0].1.overlays.as_ref().unwrap();
        assert_eq!(schema.schema().keys().collect::<Vec<_>>(), ["sentiment"]);
    }

    /// The fold is a *pre*-pass, so the rows still merge in the order the
    /// `--series` flags were given and "the later series wins a column clash"
    /// is unchanged. Folding after the inserts would have made the untagged
    /// side win regardless of where it was typed.
    #[test]
    fn folding_untagged_rows_preserves_series_order() {
        let tagged = tmp_csv(
            "fugazi_cadence_order_a.csv",
            "symbol,freq,time,open,high,low,close,volume,note\n\
             BTC,1d,2024-01-01T00:00:00Z,100,100,100,100,1,tagged\n",
        );
        let untagged = tmp_csv(
            "fugazi_cadence_order_b.csv",
            "symbol,time,note\nBTC,2024-01-01T00:00:00Z,untagged\n",
        );
        let key = (
            "BTC".to_string(),
            "1d".to_string(),
            "2024-01-01T00:00:00Z".to_string(),
        );

        let untagged_last = DataFrame::from_series(&[
            format!("@{tagged}").parse().unwrap(),
            format!("@{untagged}").parse().unwrap(),
        ])
        .unwrap();
        assert_eq!(untagged_last.rows[&key]["note"], "untagged");

        let tagged_last = DataFrame::from_series(&[
            format!("@{untagged}").parse().unwrap(),
            format!("@{tagged}").parse().unwrap(),
        ])
        .unwrap();
        assert_eq!(tagged_last.rows[&key]["note"], "tagged");
    }

    /// With two declared cadences there is no sole cadence to adopt, so the
    /// untagged rows stay in their own group for the census to report. Guessing
    /// one would re-create the silent-merge bug in a new place.
    #[test]
    fn untagged_rows_stay_apart_when_the_symbol_declares_two_cadences() {
        let path = tmp_csv(
            "fugazi_cadence_no_fold.csv",
            "symbol,freq,time,open,high,low,close,volume\n\
             BTC,1d,2024-01-01T00:00:00Z,100,100,100,100,1\n\
             BTC,1h,2024-01-01T01:00:00Z,50,50,50,50,1\n",
        );
        let overlay = tmp_csv(
            "fugazi_cadence_no_fold_ov.csv",
            "symbol,time,sentiment\nBTC,2024-01-01T00:00:00Z,0.5\n",
        );
        let frame = DataFrame::from_series(&[
            format!("@{path}").parse().unwrap(),
            format!("@{overlay}").parse().unwrap(),
        ])
        .unwrap();
        assert_eq!(frame.frequencies_of("BTC"), ["", "1d", "1h"]);
    }

    /// The fold is per symbol: BTC's sole cadence says nothing about ETH's.
    #[test]
    fn the_untagged_fold_does_not_leak_across_symbols() {
        let path = tmp_csv(
            "fugazi_cadence_fold_scope.csv",
            "symbol,freq,time,open,high,low,close,volume\n\
             BTC,1d,2024-01-01T00:00:00Z,100,100,100,100,1\n\
             ETH,,2024-01-01T00:00:00Z,9,9,9,9,1\n",
        );
        let frame = DataFrame::from_series(&[format!("@{path}").parse().unwrap()]).unwrap();
        assert_eq!(frame.frequencies_of("BTC"), ["1d"]);
        assert_eq!(frame.frequencies_of("ETH"), [""]);
    }

    /// `1M` is a month and `1m` is a minute — the `freq` cell is the one place
    /// in this loader where case carries meaning, so it is never folded.
    #[test]
    fn the_freq_cell_is_not_case_folded() {
        let path = tmp_csv(
            "fugazi_cadence_case.csv",
            "symbol,freq,time,open,high,low,close,volume\n\
             BTC,1M,2024-01-01T00:00:00Z,100,100,100,100,1\n\
             BTC,1m,2024-01-01T00:01:00Z,50,50,50,50,1\n",
        );
        let frame = DataFrame::from_series(&[format!("@{path}").parse().unwrap()]).unwrap();
        assert_eq!(frame.frequencies_of("BTC"), ["1M", "1m"]);
    }

    /// A blank `freq` cell is a missing label, not a cadence named "".
    #[test]
    fn a_blank_freq_cell_is_treated_as_absent() {
        let path = tmp_csv(
            "fugazi_cadence_blank.csv",
            "symbol,freq,time,open,high,low,close,volume\n\
             BTC,  ,2024-01-01T00:00:00Z,100,100,100,100,1\n\
             BTC,1d,2024-01-02T00:00:00Z,101,101,101,101,1\n",
        );
        let frame = DataFrame::from_series(&[format!("@{path}").parse().unwrap()]).unwrap();
        assert_eq!(frame.frequencies_of("BTC"), ["1d"]);
    }

    #[test]
    fn declared_frequency_reads_a_sole_parseable_label() {
        let path = tmp_csv(
            "fugazi_cadence_declared.csv",
            "symbol,freq,time,open,high,low,close,volume\n\
             BTC,4h,2024-01-01T00:00:00Z,100,100,100,100,1\n\
             ETH,weekly,2024-01-01T00:00:00Z,9,9,9,9,1\n\
             SOL,,2024-01-01T00:00:00Z,9,9,9,9,1\n",
        );
        let frame = DataFrame::from_series(&[format!("@{path}").parse().unwrap()]).unwrap();
        assert_eq!(frame.declared_frequency("BTC"), Some(Frequency::Hour(4)));
        // Labelled, but not as anything `Frequency` knows.
        assert_eq!(frame.declared_frequency("ETH"), None);
        assert_eq!(frame.declared_frequency("SOL"), None);
        assert_eq!(frame.declared_frequency("DOGE"), None);
    }

    /// Ambiguity has no declared cadence to report — asking a symbol that
    /// carries two would otherwise hand back whichever sorted first.
    #[test]
    fn declared_frequency_is_none_while_a_symbol_is_ambiguous() {
        let path = tmp_csv(
            "fugazi_cadence_declared_ambiguous.csv",
            "symbol,freq,time,open,high,low,close,volume\n\
             BTC,1d,2024-01-01T00:00:00Z,100,100,100,100,1\n\
             BTC,1h,2024-01-01T01:00:00Z,50,50,50,50,1\n",
        );
        let mut frame = DataFrame::from_series(&[format!("@{path}").parse().unwrap()]).unwrap();
        assert_eq!(frame.declared_frequency("BTC"), None);
        // Once resolved, the survivor answers.
        frame.retain_cadence("BTC", "1h");
        assert_eq!(frame.declared_frequency("BTC"), Some(Frequency::Hour(1)));
    }

    #[test]
    fn retain_cadence_prunes_only_the_named_symbol() {
        let path = tmp_csv(
            "fugazi_cadence_retain.csv",
            "symbol,freq,time,open,high,low,close,volume\n\
             BTC,1d,2024-01-01T00:00:00Z,100,100,100,100,1\n\
             BTC,1h,2024-01-01T01:00:00Z,50,50,50,50,1\n\
             ETH,1h,2024-01-01T01:00:00Z,9,9,9,9,1\n",
        );
        let mut frame = DataFrame::from_series(&[format!("@{path}").parse().unwrap()]).unwrap();
        frame.retain_cadence("BTC", "1d");
        assert_eq!(frame.frequencies_of("BTC"), ["1d"]);
        assert_eq!(frame.frequencies_of("ETH"), ["1h"]);
        assert_eq!(frame.atoms("BTC").unwrap().atoms.len(), 1);
    }

    /// Pruning can retire the last row carrying a column, so the memoized
    /// frame-wide schema has to be rebuilt — a stale `Arc` here would leave
    /// every `!get` resolving against columns the run no longer has.
    #[test]
    fn retain_cadence_rebuilds_the_memoized_schema() {
        let path = tmp_csv(
            "fugazi_cadence_retain_schema.csv",
            "symbol,freq,time,open,high,low,close,volume,funding\n\
             BTC,1d,2024-01-01T00:00:00Z,100,100,100,100,1,\n\
             BTC,8h,2024-01-01T08:00:00Z,50,50,50,50,1,0.01\n",
        );
        let mut frame = DataFrame::from_series(&[format!("@{path}").parse().unwrap()]).unwrap();
        // Memoize it first, so the test really exercises the invalidation.
        assert!(frame.shared_schema().is_some());
        frame.retain_cadence("BTC", "1d");
        let schema = frame.shared_schema().expect("the column still exists");
        assert_eq!(schema.keys().collect::<Vec<_>>(), ["funding"]);
        assert_eq!(frame.atoms("BTC").unwrap().atoms.len(), 1);
    }

    #[test]
    fn cadence_groups_sorts_stamps_and_skips_unparseable_times() {
        let path = tmp_csv(
            "fugazi_cadence_groups.csv",
            "symbol,freq,time,open,high,low,close,volume\n\
             BTC,1d,1704153600,100,100,100,100,1\n\
             BTC,1d,1704067200,100,100,100,100,1\n\
             BTC,1d,not-a-time,100,100,100,100,1\n",
        );
        let frame = DataFrame::from_series(&[format!("@{path}").parse().unwrap()]).unwrap();
        let groups = frame.cadence_groups();
        assert_eq!(groups.len(), 1);
        let (symbol, freq, stamps) = &groups[0];
        assert_eq!((symbol.as_str(), freq.as_str()), ("BTC", "1d"));
        // Epoch seconds sort lexicographically the wrong way round in the key;
        // the census gets them ascending, and the unparseable row is absent
        // rather than counted as a bar it cannot space.
        assert_eq!(stamps, &[1_704_067_200_000, 1_704_153_600_000]);
    }

    #[test]
    fn cadence_groups_separates_every_symbol_and_label() {
        let path = tmp_csv(
            "fugazi_cadence_groups_split.csv",
            "symbol,freq,time,open,high,low,close,volume\n\
             BTC,1d,2024-01-01T00:00:00Z,100,100,100,100,1\n\
             BTC,1h,2024-01-01T01:00:00Z,50,50,50,50,1\n\
             ETH,1d,2024-01-01T00:00:00Z,9,9,9,9,1\n",
        );
        let frame = DataFrame::from_series(&[format!("@{path}").parse().unwrap()]).unwrap();
        let seen: Vec<(String, String, usize)> = frame
            .cadence_groups()
            .into_iter()
            .map(|(s, f, stamps)| (s, f, stamps.len()))
            .collect();
        assert_eq!(
            seen,
            [
                ("BTC".to_string(), "1d".to_string(), 1),
                ("BTC".to_string(), "1h".to_string(), 1),
                ("ETH".to_string(), "1d".to_string(), 1),
            ]
        );
    }

    /// `"nan"`, `"NaN"`, `"inf"` and `"-inf"` all parse as an `f64`, and a
    /// `Candle` is five bare `Real`s with no `None` to fall back to. One such
    /// cell used to poison the whole run silently — the position marked at a
    /// `NaN`, equity went `NaN`, and the report printed `return NaN% ann`
    /// beside a plausible fill list. A genuine gap is a row that is absent, not
    /// a row that says `NaN`.
    #[test]
    fn a_non_finite_price_is_refused_rather_than_marked() {
        for bad in ["NaN", "nan", "inf", "-inf", "Infinity"] {
            let path = tmp_csv(
                "fugazi_nonfinite.csv",
                &format!("symbol;time;open;high;low;close;volume\nBTC;1;10;11;9;{bad};100\n"),
            );
            let frame = DataFrame::from_series(&[format!("@{path}").parse().unwrap()])
                .expect("the frame loads; the candle is parsed on demand");
            let err = frame
                .atoms("BTC")
                .expect_err(&format!("`{bad}` must be refused as a close"));
            let msg = format!("{err:#}");
            assert!(
                msg.contains("finite"),
                "the error should say why, got: {msg}"
            );
        }
        // A non-finite *volume* is refused on the same footing.
        let path = tmp_csv(
            "fugazi_nonfinite_vol.csv",
            "symbol;time;open;high;low;close;volume\nBTC;1;10;11;9;10.5;NaN\n",
        );
        let frame = DataFrame::from_series(&[format!("@{path}").parse().unwrap()]).unwrap();
        assert!(frame.atoms("BTC").is_err());

        // An ordinary row is untouched, and an empty volume still defaults.
        let path = tmp_csv(
            "fugazi_finite.csv",
            "symbol;time;open;high;low;close;volume\nBTC;1;10;11;9;10.5;\n",
        );
        let frame = DataFrame::from_series(&[format!("@{path}").parse().unwrap()]).unwrap();
        assert_eq!(frame.atoms("BTC").unwrap().atoms.len(), 1);
    }
}
