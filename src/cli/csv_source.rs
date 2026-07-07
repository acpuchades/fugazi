//! `csv:PATH` provider for `fugazi get` — reads OHLCV bars from a local CSV
//! (typically one previously produced by `fugazi get`). The file's `symbol`,
//! `freq`, `time`, `open`, `high`, `low`, `close` and `volume` columns become
//! each bar's [`Candle`]; every other column becomes a `Real`/`Bool`/`Str`
//! entry on the atom's overlay side channel, so a downstream `!get { key }`
//! reference resolves the same way a `--series` load would. Delimiter is
//! autodetected from the header (`;`, `,`, `\t`, `|`) — the same rule
//! `--series` follows.
//!
//! Unlike the remote providers, `csv:` doesn't fit the standard
//! `provider:SYMBOL[freq]` spec grammar (the file already carries symbol+freq
//! per row), so `get.rs` special-cases the `csv:` prefix: after the colon the
//! whole remainder is the path, and enumeration of the file's own
//! `(symbol, interval)` combinations drives the per-series pipeline.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use fugazi::prelude::*;
use fugazi::sources::{Interval, Timestamp};

use super::calendar::{parse_interval, parse_time_to_millis};

/// A local CSV file of OHLCV bars, in the shape [`fugazi get`] itself writes.
pub struct CsvSource {
    path: PathBuf,
}

/// One row from the file: the promoted `symbol`/`freq` fields plus a fully
/// populated [`Atom`] carrying the OHLCV candle, the bar-open [`Timestamp`],
/// and — when the file has any non-OHLCV columns — an [`OverlayInfo`] side
/// channel whose [`Schema`] is column-classified across the whole file
/// (**Bool > Real > Str** priority, matching [`crate::data::DataFrame::atoms`]).
///
/// Every atom in a single `read()` shares the same `Arc<Schema>`, so a
/// downstream consumer can grab it off any atom via
/// [`fugazi::sources::schema_of`] without re-scanning the file.
#[derive(Debug, Clone)]
pub struct CsvBar {
    pub symbol: String,
    pub interval: Interval,
    pub atom: Atom,
}

/// Column-classifier state: two flags start `true` and monotonically flip to
/// `false` on the first observation that violates them. After every row is
/// observed the type is picked in priority order **Bool > Real > Str**
/// (both flags true → Bool; only `real_ok` → Real; otherwise → Str). Mirrors
/// `crate::data::ColumnState`.
#[derive(Debug, Clone, Copy)]
struct ColumnState {
    bool_ok: bool,
    real_ok: bool,
    seen_any: bool,
}

impl ColumnState {
    fn new() -> Self {
        Self { bool_ok: true, real_ok: true, seen_any: false }
    }

    fn observe(&mut self, value: &str) {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return;
        }
        self.seen_any = true;
        if !is_bool_token(trimmed) {
            self.bool_ok = false;
        }
        if trimmed.parse::<Real>().is_err() {
            self.real_ok = false;
        }
    }

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

fn is_bool_token(s: &str) -> bool {
    s.eq_ignore_ascii_case("true") || s.eq_ignore_ascii_case("false")
}

/// Buffered representation of one parsed CSV row before the second (typing)
/// pass materializes it into a [`CsvBar`]. Extras stay as raw strings until
/// the schema is finalized; that keeps type coercion consistent with what
/// [`crate::data::DataFrame::atoms`] does for `--series` loads.
struct RawRow {
    symbol: String,
    interval: Interval,
    time_ms: i64,
    candle: Candle,
    extras: Vec<String>, // aligned with the schema's column order
}

impl CsvSource {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Read every OHLCV row in the file, populating each atom with time +
    /// overlays. `symbol`, `freq`, `time`, `open`, `high`, `low`, `close` are
    /// required columns; `volume` defaults to `0` when missing or blank.
    /// Every non-reserved header becomes a schema column; its type is
    /// classified across the whole file (**Bool > Real > Str** priority),
    /// then each row's cell is coerced against that column's declared type.
    pub fn read(&self) -> Result<Vec<CsvBar>> {
        let delimiter = detect_delimiter(&self.path)?;
        let mut reader = csv::ReaderBuilder::new()
            .delimiter(delimiter)
            .from_path(&self.path)
            .with_context(|| format!("opening {}", self.path.display()))?;
        let headers: Vec<String> = reader
            .headers()
            .with_context(|| format!("reading header of {}", self.path.display()))?
            .iter()
            .map(|h| h.trim().to_lowercase())
            .collect();
        let path = self.path.display().to_string();
        let idx = |name: &str| {
            headers
                .iter()
                .position(|h| h == name)
                .ok_or_else(|| anyhow!("{path}: missing required column `{name}`"))
        };
        let i_symbol = idx("symbol")?;
        let i_freq = idx("freq")?;
        let i_time = idx("time")?;
        let i_open = idx("open")?;
        let i_high = idx("high")?;
        let i_low = idx("low")?;
        let i_close = idx("close")?;
        let i_volume = headers.iter().position(|h| h == "volume");

        // Non-OHLCV column indexes, in header order — these drive the atom's
        // overlay Schema and every row's overlay values.
        let reserved: std::collections::HashSet<&str> =
            ["symbol", "freq", "time", "open", "high", "low", "close", "volume"]
                .into_iter()
                .collect();
        let extra_columns: Vec<(usize, String)> = headers
            .iter()
            .enumerate()
            .filter(|(_, name)| !reserved.contains(name.as_str()))
            .map(|(i, name)| (i, name.clone()))
            .collect();

        // Pass 1: parse each row, buffering OHLCV/time plus the raw extras
        // strings, while classifying every extra column across the whole file.
        let mut classification: Vec<ColumnState> =
            extra_columns.iter().map(|_| ColumnState::new()).collect();
        let mut rows: Vec<RawRow> = Vec::new();
        for (line_no, record) in reader.records().enumerate() {
            let line = line_no + 2; // header is line 1
            let record =
                record.with_context(|| format!("{path}: reading row {line}"))?;
            let field = |i: usize| record.get(i).unwrap_or("").trim();
            let parse_real = |i: usize, name: &str| -> Result<Real> {
                let raw = field(i);
                raw.parse::<Real>()
                    .with_context(|| format!("{path}: row {line}: column `{name}` = {raw:?}"))
            };
            let interval = parse_interval(field(i_freq))
                .with_context(|| format!("{path}: row {line}: column `freq`"))?;
            let time_ms = parse_time_to_millis(field(i_time)).ok_or_else(|| {
                anyhow!(
                    "{path}: row {line}: column `time` = {:?} — expected RFC3339, `YYYY-MM-DD [HH:MM:SS]`, or an epoch stamp in seconds/millis",
                    field(i_time)
                )
            })?;
            let volume = match i_volume {
                Some(i) => {
                    let raw = field(i);
                    if raw.is_empty() {
                        0.0
                    } else {
                        raw.parse::<Real>().with_context(|| {
                            format!("{path}: row {line}: column `volume` = {raw:?}")
                        })?
                    }
                }
                None => 0.0,
            };
            let candle = Candle::new(
                parse_real(i_open, "open")?,
                parse_real(i_high, "high")?,
                parse_real(i_low, "low")?,
                parse_real(i_close, "close")?,
                volume,
            );
            let symbol = field(i_symbol).to_string();
            if symbol.is_empty() {
                return Err(anyhow!("{path}: row {line}: column `symbol` is empty"));
            }
            let mut extras: Vec<String> = Vec::with_capacity(extra_columns.len());
            for (slot, (i, _)) in extra_columns.iter().enumerate() {
                let raw = field(*i).to_string();
                classification[slot].observe(&raw);
                extras.push(raw);
            }
            rows.push(RawRow { symbol, interval, time_ms, candle, extras });
        }

        // Build the shared schema once — every atom's OverlayInfo references
        // the same `Arc<Schema>`, so a downstream `sources::schema_of` picks
        // it off any atom in O(1).
        let schema: Option<Arc<Schema>> = if extra_columns.is_empty() {
            None
        } else {
            let mut b = Schema::builder();
            for ((_, name), state) in extra_columns.iter().zip(classification.iter()) {
                match state.resolve() {
                    OverlayType::Real => {
                        b.add_real(name.clone());
                    }
                    OverlayType::Bool => {
                        b.add_bool(name.clone());
                    }
                    OverlayType::Str => {
                        b.add_str(name.clone());
                    }
                }
            }
            Some(b.finish())
        };

        // Pass 2: materialize each RawRow into a fully-populated Atom.
        let mut out: Vec<CsvBar> = Vec::with_capacity(rows.len());
        for row in rows {
            let atom = match &schema {
                None => Atom::with_time(row.candle, Timestamp(row.time_ms)),
                Some(schema) => {
                    let values: Vec<OverlayValue> = row
                        .extras
                        .iter()
                        .enumerate()
                        .map(|(i, raw)| {
                            cell_to_overlay(
                                raw,
                                schema.type_of(i).expect("schema built with N columns"),
                            )
                        })
                        .collect();
                    let overlays = OverlayInfo::new(schema.clone(), values);
                    Atom::with_overlays_and_time(row.candle, overlays, Timestamp(row.time_ms))
                }
            };
            out.push(CsvBar { symbol: row.symbol, interval: row.interval, atom });
        }
        Ok(out)
    }
}

/// Coerce one raw cell to the schema-declared type for its column. Missing /
/// empty cells fall through to type-appropriate defaults (`Real::NAN` for
/// `Real`, `false` for `Bool`, `""` for `Str`) — same convention
/// [`crate::data::DataFrame::atoms`] uses.
fn cell_to_overlay(raw: &str, ty: OverlayType) -> OverlayValue {
    let trimmed = raw.trim();
    match ty {
        OverlayType::Real => {
            if trimmed.is_empty() {
                OverlayValue::Real(Real::NAN)
            } else {
                OverlayValue::Real(trimmed.parse::<Real>().unwrap_or(Real::NAN))
            }
        }
        OverlayType::Bool => {
            if trimmed.is_empty() {
                OverlayValue::Bool(false)
            } else {
                OverlayValue::Bool(trimmed.eq_ignore_ascii_case("true"))
            }
        }
        OverlayType::Str => OverlayValue::Str(Arc::from(trimmed)),
    }
}

/// Bin classified column types into an ordered `(name, OverlayType)` list —
/// convenience for tests that want to inspect the resolved schema without
/// dragging in the full CsvBar.
#[cfg(test)]
fn classified_types(bars: &[CsvBar]) -> Vec<(String, OverlayType)> {
    let atoms: Vec<Atom> = bars.iter().map(|b| b.atom.clone()).collect();
    let schema = fugazi::sources::schema_of(&atoms);
    schema
        .keys()
        .enumerate()
        .map(|(i, name)| (name.to_string(), schema.type_of(i).unwrap()))
        .collect()
}

/// Guess a CSV's column delimiter from its header line: whichever of `; , \t |`
/// occurs most often wins (ties favour earlier in that list); a single-column
/// file with none of them falls back to `,`. Used by both the `csv:` source
/// here and the `--series` path in [`crate::data`], so both read the same
/// files identically.
pub(crate) fn detect_delimiter(path: &Path) -> Result<u8> {
    const CANDIDATES: [u8; 4] = [b';', b',', b'\t', b'|'];
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut header = String::new();
    BufReader::new(file)
        .read_line(&mut header)
        .with_context(|| format!("reading header of {}", path.display()))?;

    let mut best = (b',', 0usize);
    for d in CANDIDATES {
        let n = header.bytes().filter(|&b| b == d).count();
        if n > best.1 {
            best = (d, n);
        }
    }
    Ok(best.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp_csv(name: &str, contents: &str) -> PathBuf {
        let dir = std::env::temp_dir();
        let path = dir.join(name);
        let mut f = File::create(&path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        path
    }

    #[test]
    fn reads_semicolon_csv_in_get_output_shape() {
        let path = tmp_csv(
            "fugazi_csv_source_test_a.csv",
            "symbol;freq;time;open;high;low;close;volume\n\
             BTCUSDT;1d;2024-01-01T00:00:00Z;100;110;90;105;1000\n\
             BTCUSDT;1d;2024-01-02T00:00:00Z;105;115;100;108;1200\n",
        );
        let bars = CsvSource::new(path).read().unwrap();
        assert_eq!(bars.len(), 2);
        assert_eq!(bars[0].symbol, "BTCUSDT");
        assert_eq!(bars[0].interval, Interval::Day(1));
        assert_eq!(bars[0].atom.candle.close, 105.0);
        assert_eq!(bars[0].atom.time, Some(Timestamp(1_704_067_200_000)));
        assert_eq!(bars[1].atom.candle.volume, 1200.0);
    }

    #[test]
    fn autodetects_comma_delimiter() {
        let path = tmp_csv(
            "fugazi_csv_source_test_b.csv",
            "symbol,freq,time,open,high,low,close,volume\n\
             ETHUSDT,1h,2024-01-01T00:00:00Z,10,11,9,10.5,50\n",
        );
        let bars = CsvSource::new(path).read().unwrap();
        assert_eq!(bars.len(), 1);
        assert_eq!(bars[0].interval, Interval::Hour(1));
    }

    #[test]
    fn tolerates_missing_volume_column_and_blank_volume_cell() {
        let path = tmp_csv(
            "fugazi_csv_source_test_c.csv",
            "symbol;freq;time;open;high;low;close\n\
             AAA;1d;2024-01-01T00:00:00Z;1;2;0.5;1.5\n",
        );
        let bars = CsvSource::new(path).read().unwrap();
        assert_eq!(bars[0].atom.candle.volume, 0.0);

        let path = tmp_csv(
            "fugazi_csv_source_test_d.csv",
            "symbol;freq;time;open;high;low;close;volume\n\
             AAA;1d;2024-01-01T00:00:00Z;1;2;0.5;1.5;\n",
        );
        let bars = CsvSource::new(path).read().unwrap();
        assert_eq!(bars[0].atom.candle.volume, 0.0);
    }

    #[test]
    fn accepts_millisecond_epoch_time() {
        let path = tmp_csv(
            "fugazi_csv_source_test_e.csv",
            "symbol;freq;time;open;high;low;close;volume\n\
             AAA;1d;1704067200000;1;2;0.5;1.5;10\n",
        );
        let bars = CsvSource::new(path).read().unwrap();
        assert_eq!(bars[0].atom.time, Some(Timestamp(1_704_067_200_000)));
    }

    #[test]
    fn preserves_extra_columns_as_typed_overlays() {
        let path = tmp_csv(
            "fugazi_csv_source_test_f.csv",
            "symbol;freq;time;open;high;low;close;volume;sma20;risk_on;regime\n\
             AAA;1d;2024-01-01T00:00:00Z;1;2;0.5;1.5;10;1.4;true;bull\n\
             AAA;1d;2024-01-02T00:00:00Z;1.5;2.5;1;2;12;1.6;false;bear\n",
        );
        let bars = CsvSource::new(path).read().unwrap();
        assert_eq!(bars.len(), 2);
        assert_eq!(bars[0].atom.candle.close, 1.5);
        // Non-OHLCV columns survive as a typed schema, column-classified
        // across the whole file (sma20 → Real, risk_on → Bool, regime → Str).
        let types = classified_types(&bars);
        let find = |name: &str| types.iter().find(|(n, _)| n == name).map(|(_, t)| *t);
        assert_eq!(find("sma20"), Some(OverlayType::Real));
        assert_eq!(find("risk_on"), Some(OverlayType::Bool));
        assert_eq!(find("regime"), Some(OverlayType::Str));
        let overlays = bars[0].atom.overlays.as_ref().expect("overlays present");
        assert_eq!(overlays.get_by_key("sma20"), Some(&OverlayValue::Real(1.4)));
        assert_eq!(overlays.get_by_key("risk_on"), Some(&OverlayValue::Bool(true)));
        assert!(matches!(
            overlays.get_by_key("regime"),
            Some(OverlayValue::Str(s)) if s.as_ref() == "bull"
        ));
    }

    #[test]
    fn rejects_missing_required_column() {
        let path = tmp_csv(
            "fugazi_csv_source_test_g.csv",
            "symbol;time;open;high;low;close;volume\n",
        );
        let err = CsvSource::new(path).read().unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("missing required column `freq`"), "{msg}");
    }

    #[test]
    fn rejects_bad_freq_token() {
        let path = tmp_csv(
            "fugazi_csv_source_test_h.csv",
            "symbol;freq;time;open;high;low;close;volume\n\
             AAA;1x;2024-01-01T00:00:00Z;1;2;0.5;1.5;10\n",
        );
        let err = CsvSource::new(path).read().unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("column `freq`"), "{msg}");
    }
}
