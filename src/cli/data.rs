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
//! **full-outer-joined on `(symbol, time)`** into one long dataframe: a
//! `BTreeMap` keyed by `(symbol, time)`, so iteration is ascending by symbol then
//! by `time` — and `time` is compared as the opaque, caller-sorted string it was
//! given (dates, epochs, anything).

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::str::FromStr;

use anyhow::{Context, Result, anyhow, bail};
use fugazi::prelude::*;

/// A column-keyed row; column names are lowercased for case-insensitive lookup.
type Row = HashMap<String, String>;

/// Columns treated as OHLCV or metadata and therefore never lifted into an
/// overlay schema. Everything else in a row is a candidate overlay column.
const RESERVED_COLUMNS: &[&str] = &[
    "symbol", "time", "freq", "open", "high", "low", "close", "volume",
];

/// Classification of a candidate overlay column across a symbol's rows: whether
/// its observed values are all numeric, mixed with a non-numeric one, or never
/// present at all. Sticky — once `NonNumeric`, it stays that way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColumnState {
    /// No non-empty value observed yet.
    Empty,
    /// Every non-empty value observed parses as [`Real`].
    Numeric,
    /// At least one non-empty value failed to parse.
    NonNumeric,
}

/// The atom series for one symbol: per-bar `(time, atom)` pairs plus the list
/// of non-numeric overlay columns dropped from the shared [`Schema`] (which
/// callers surface as a warning).
#[derive(Debug)]
pub struct AtomSeries {
    /// One `(time, atom)` per bar, ascending by `time`.
    pub atoms: Vec<(String, Atom)>,
    /// Non-reserved columns that carried at least one non-numeric value and
    /// were therefore excluded from the schema. Alphabetical.
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

/// The merged long dataframe: rows keyed by `(symbol, time)`.
#[derive(Debug, Default)]
pub struct DataFrame {
    rows: BTreeMap<(String, String), Row>,
}

impl DataFrame {
    /// Build the dataframe from the parsed `--series` specs. Each `@file`'s column
    /// delimiter is autodetected from its header.
    pub fn from_series(series: &[SeriesSpec]) -> Result<Self> {
        let mut frame = DataFrame::default();
        for spec in series {
            for row in spec.rows()? {
                frame.insert(&spec.raw, row)?;
            }
        }
        Ok(frame)
    }

    /// Merge one row into the frame, joining on `(symbol, time)`.
    fn insert(&mut self, spec: &str, row: Row) -> Result<()> {
        let symbol = row
            .get("symbol")
            .cloned()
            .ok_or_else(|| anyhow!("series `{spec}`: a row is missing a `symbol` column"))?;
        let time = row
            .get("time")
            .cloned()
            .ok_or_else(|| anyhow!("series `{spec}`: a row is missing a `time` column"))?;
        self.rows.entry((symbol, time)).or_default().extend(row);
        Ok(())
    }

    /// Return the ascending `time` list of the largest `(symbol, freq)`
    /// series matching `symbol`. When a symbol has multiple `freq` groups
    /// — e.g. a 1d and a 1h stream in the same frame — the one with the
    /// most rows wins, since it is almost always the primary series the
    /// strategy is trading. Rows with no `freq` column bucket together
    /// under `None`. Returns `None` when the frame carries nothing for
    /// `symbol`. Used by the calendar auto-detection so different cadences
    /// of the same symbol aren't averaged into one median.
    pub fn dominant_series_times(&self, symbol: &str) -> Option<Vec<&str>> {
        let mut buckets: BTreeMap<Option<String>, Vec<&str>> = BTreeMap::new();
        for ((sym, time), row) in &self.rows {
            if sym != symbol {
                continue;
            }
            let freq = row.get("freq").filter(|s| !s.is_empty()).cloned();
            buckets.entry(freq).or_default().push(time.as_str());
        }
        buckets
            .into_values()
            .max_by_key(|times| times.len())
    }

    /// The atom series for `symbol`, ascending by `time`: OHLCV candles plus
    /// per-bar overlay values keyed by a shared [`Schema`] built from every
    /// non-reserved column found in the symbol's rows.
    ///
    /// A candidate overlay column is included in the schema iff every non-empty
    /// value for it parses as [`Real`]; columns with at least one non-numeric
    /// value are dropped from the schema and returned in
    /// [`AtomSeries::skipped_columns`] so the caller can warn. Missing values
    /// (row lacks the column, or the cell is empty) become [`Real::NAN`].
    /// Schema columns are ordered alphabetically for determinism.
    pub fn atoms(&self, symbol: &str) -> Result<AtomSeries> {
        // Single pass over the symbol's rows to (a) confirm the symbol has
        // rows at all and (b) classify each non-reserved column by
        // parseability across its observed values.
        let mut classification: BTreeMap<String, ColumnState> = BTreeMap::new();
        let mut any_row = false;
        for ((sym, _time), row) in &self.rows {
            if sym != symbol {
                continue;
            }
            any_row = true;
            for (name, value) in row {
                if RESERVED_COLUMNS.contains(&name.as_str()) {
                    continue;
                }
                let state = classification
                    .entry(name.clone())
                    .or_insert(ColumnState::Empty);
                if *state == ColumnState::NonNumeric {
                    continue; // sticky — one bad value is enough
                }
                let trimmed = value.trim();
                if trimmed.is_empty() {
                    continue; // missing carries no signal about the column type
                }
                *state = if trimmed.parse::<Real>().is_ok() {
                    ColumnState::Numeric
                } else {
                    ColumnState::NonNumeric
                };
            }
        }

        if !any_row {
            bail!("no rows found for symbol `{symbol}` across the given --series");
        }

        // BTreeMap iterates alphabetically, so numeric_columns and
        // skipped_columns come out sorted for free.
        let numeric_columns: Vec<String> = classification
            .iter()
            .filter(|(_, s)| **s == ColumnState::Numeric)
            .map(|(k, _)| k.clone())
            .collect();
        let skipped_columns: Vec<String> = classification
            .iter()
            .filter(|(_, s)| **s == ColumnState::NonNumeric)
            .map(|(k, _)| k.clone())
            .collect();

        let schema = if numeric_columns.is_empty() {
            None
        } else {
            let mut b = Schema::builder();
            for name in &numeric_columns {
                b.add(name.clone());
            }
            Some(b.finish())
        };

        // Second pass: build one atom per row, attaching overlays when the
        // schema has any columns.
        let mut atoms = Vec::new();
        for ((sym, time), row) in &self.rows {
            if sym != symbol {
                continue;
            }
            let candle = row_to_candle(sym, time, row)?;
            let atom = match &schema {
                None => Atom::new(candle),
                Some(schema) => {
                    let values: Vec<Real> = numeric_columns
                        .iter()
                        .map(|name| {
                            row.get(name)
                                .map(|v| v.trim())
                                .filter(|v| !v.is_empty())
                                .and_then(|v| v.parse::<Real>().ok())
                                .unwrap_or(Real::NAN)
                        })
                        .collect();
                    let overlays = OverlayInfo::new(schema.clone(), values);
                    Atom::with_overlays(candle, overlays)
                }
            };
            atoms.push((time.clone(), atom));
        }

        Ok(AtomSeries {
            atoms,
            skipped_columns,
        })
    }
}

/// Build a [`Candle`] from one row's OHLCV columns.
fn row_to_candle(sym: &str, time: &str, row: &Row) -> Result<Candle> {
    let field = |name: &str| -> Result<Real> {
        let raw = row
            .get(name)
            .ok_or_else(|| anyhow!("{sym} @ {time}: missing required column `{name}`"))?;
        raw.parse::<Real>()
            .with_context(|| format!("{sym} @ {time}: column `{name}` = {raw:?}"))
    };
    let volume = match row.get("volume") {
        Some(raw) if !raw.is_empty() => raw
            .parse::<Real>()
            .with_context(|| format!("{sym} @ {time}: column `volume` = {raw:?}"))?,
        _ => 0.0,
    };
    Ok(Candle::new(
        field("open")?,
        field("high")?,
        field("low")?,
        field("close")?,
        volume,
    ))
}

/// Read a CSV file into lowercased-column rows, autodetecting its delimiter.
fn read_csv(path: &str) -> Result<Vec<Row>> {
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(detect_delimiter(path)?)
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

/// Guess a CSV's column delimiter from its header line: whichever of `; , \t |`
/// occurs most often wins (ties favour earlier in that list); a single-column
/// file with none of them falls back to `,`.
fn detect_delimiter(path: &str) -> Result<u8> {
    use std::io::BufRead;

    const CANDIDATES: [u8; 4] = [b';', b',', b'\t', b'|'];
    let file = std::fs::File::open(path).with_context(|| format!("opening CSV `{path}`"))?;
    let mut header = String::new();
    std::io::BufReader::new(file)
        .read_line(&mut header)
        .with_context(|| format!("reading header of `{path}`"))?;

    let mut best = (b',', 0usize);
    for d in CANDIDATES {
        let n = header.bytes().filter(|&b| b == d).count();
        if n > best.1 {
            best = (d, n);
        }
    }
    Ok(best.0)
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

    fn tmp_csv(name: &str, contents: &str) -> String {
        let dir = std::env::temp_dir();
        let path = dir.join(name);
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
        let frame = DataFrame::from_series(&[format!("symbol='BTC',@{path}").parse().unwrap()]).unwrap();
        let series = frame.atoms("BTC").unwrap();
        assert_eq!(series.atoms.len(), 2);
        assert_eq!(series.atoms[0].0, "1");
        assert_eq!(series.atoms[0].1.candle.close, 10.5);
        assert_eq!(series.atoms[1].1.candle.high, 12.0);
    }

    #[test]
    fn two_series_full_join_on_symbol_time() {
        let prices = tmp_csv(
            "fugazi_data_test_p.csv",
            "time;open;high;low;close\n1;10;11;9;10\n2;10;12;10;11\n",
        );
        let fundamentals = tmp_csv(
            "fugazi_data_test_f.csv",
            "time;pe_ratio\n1;15.0\n2;16.0\n",
        );
        let frame = DataFrame::from_series(&[
            format!("symbol=BTC,@{prices}").parse().unwrap(),
            format!("symbol=BTC,@{fundamentals}").parse().unwrap(),
        ])
        .unwrap();
        // The extra column rode along on the joined rows.
        assert_eq!(frame.rows[&("BTC".into(), "1".into())]["pe_ratio"], "15.0");
        // Candles still build (volume defaulted to 0).
        let series = frame.atoms("BTC").unwrap();
        assert_eq!(series.atoms.len(), 2);
        assert_eq!(series.atoms[0].1.candle.volume, 0.0);
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
        let frame =
            DataFrame::from_series(&[format!("symbol=BTC,@{p1},exchange=NYSE,@{p2}").parse().unwrap()])
                .unwrap();
        // Both files' rows concatenated.
        assert_eq!(frame.atoms("BTC").unwrap().atoms.len(), 4);
        // Both literals broadcast onto rows from either file.
        assert_eq!(frame.rows[&("BTC".into(), "1".into())]["exchange"], "NYSE");
        assert_eq!(frame.rows[&("BTC".into(), "4".into())]["exchange"], "NYSE");
    }

    #[test]
    fn series_without_a_file_is_rejected() {
        assert!("symbol=BTC".parse::<SeriesSpec>().is_err());
    }

    #[test]
    fn dominant_series_times_picks_the_largest_freq_group_for_a_symbol() {
        // Same symbol, two freqs — the larger bucket wins for auto-detection.
        let btc_1d = tmp_csv(
            "fugazi_dominant_btc_1d.csv",
            "time;freq;open;high;low;close\n2024-01-01;1d;10;11;9;10\n2024-01-02;1d;10;12;10;11\n",
        );
        let btc_1h = tmp_csv(
            "fugazi_dominant_btc_1h.csv",
            "time;freq;open;high;low;close\n\
             2024-01-01T00:00:00Z;1h;10;11;9;10\n\
             2024-01-01T01:00:00Z;1h;10;11;9;10\n\
             2024-01-01T02:00:00Z;1h;10;11;9;10\n",
        );
        let frame = DataFrame::from_series(&[
            format!("symbol=BTC,@{btc_1d}").parse().unwrap(),
            format!("symbol=BTC,@{btc_1h}").parse().unwrap(),
        ])
        .unwrap();
        let times = frame.dominant_series_times("BTC").unwrap();
        assert_eq!(times.len(), 3);
        assert!(times[0].starts_with("2024-01-01T"));
    }

    #[test]
    fn dominant_series_times_returns_none_for_unknown_symbol() {
        let path = tmp_csv(
            "fugazi_dominant_none.csv",
            "time;open;high;low;close\n2024-01-01;10;11;9;10\n",
        );
        let frame = DataFrame::from_series(&[format!("symbol=BTC,@{path}").parse().unwrap()]).unwrap();
        assert!(frame.dominant_series_times("ETH").is_none());
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
        assert_eq!(overlays.get_by_key("vol_20"), Some(0.12));
        assert_eq!(overlays.get_by_key("regime_score"), Some(1.0));
    }

    #[test]
    fn atoms_skip_non_numeric_columns_and_report_them() {
        let path = tmp_csv(
            "fugazi_atoms_nonnumeric.csv",
            "time;open;high;low;close;exchange;vol_20\n\
             1;10;11;9;10;NYSE;0.12\n\
             2;10;12;10;11;NYSE;0.15\n",
        );
        let frame =
            DataFrame::from_series(&[format!("symbol=BTC,@{path}").parse().unwrap()]).unwrap();
        let series = frame.atoms("BTC").unwrap();
        // `exchange` is non-numeric — dropped from the schema and reported.
        assert_eq!(series.skipped_columns, vec!["exchange".to_string()]);
        let overlays = series.atoms[0].1.overlays.as_ref().expect("vol_20 survives");
        let schema = overlays.schema();
        assert!(!schema.contains("exchange"));
        assert_eq!(schema.index_of("vol_20"), Some(0));
    }

    #[test]
    fn atoms_use_nan_for_missing_overlay_cells() {
        let prices = tmp_csv(
            "fugazi_atoms_prices.csv",
            "time;open;high;low;close\n1;10;11;9;10\n2;10;12;10;11\n",
        );
        // Sparse extra column: only present at time=1, missing at time=2.
        let overlay = tmp_csv(
            "fugazi_atoms_overlay.csv",
            "time;pe_ratio\n1;15.0\n",
        );
        let frame = DataFrame::from_series(&[
            format!("symbol=BTC,@{prices}").parse().unwrap(),
            format!("symbol=BTC,@{overlay}").parse().unwrap(),
        ])
        .unwrap();
        let series = frame.atoms("BTC").unwrap();
        assert_eq!(series.atoms.len(), 2);
        let overlays0 = series.atoms[0].1.overlays.as_ref().unwrap();
        let idx = overlays0.schema().index_of("pe_ratio").unwrap();

        let v0 = overlays0.get(idx).unwrap();
        let v1 = series.atoms[1].1.overlays.as_ref().unwrap().get(idx).unwrap();
        assert_eq!(v0, 15.0);
        assert!(v1.is_nan(), "missing overlay value should be NaN, got {v1}");
    }

    #[test]
    fn atoms_carry_no_overlays_when_no_numeric_column_survives() {
        // Only OHLCV + a non-numeric metadata column — no schema, no
        // OverlayInfo attached.
        let path = tmp_csv(
            "fugazi_atoms_empty_schema.csv",
            "time;open;high;low;close;exchange\n1;10;11;9;10;NYSE\n",
        );
        let frame =
            DataFrame::from_series(&[format!("symbol=BTC,@{path}").parse().unwrap()]).unwrap();
        let series = frame.atoms("BTC").unwrap();
        assert_eq!(series.skipped_columns, vec!["exchange".to_string()]);
        // No numeric overlay column survived — the atom carries no overlay info.
        assert!(series.atoms[0].1.overlays.is_none());
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
        let schema0 = series.atoms[0].1.overlays.as_ref().unwrap().schema().clone();
        for (_, atom) in &series.atoms[1..] {
            let s = atom.overlays.as_ref().unwrap().schema();
            assert!(Arc::ptr_eq(&schema0, s), "every atom must reuse the shared Arc<Schema>");
        }
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
}
