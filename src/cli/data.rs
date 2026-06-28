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

use anyhow::{Context, Result, anyhow, bail};
use fugazi::prelude::*;

/// A column-keyed row; column names are lowercased for case-insensitive lookup.
type Row = HashMap<String, String>;

/// The merged long dataframe: rows keyed by `(symbol, time)`.
#[derive(Debug, Default)]
pub struct DataFrame {
    rows: BTreeMap<(String, String), Row>,
}

impl DataFrame {
    /// Build the dataframe from the raw `--series` flag values. Each `@file`'s
    /// column delimiter is autodetected from its header.
    pub fn from_series(series: &[String]) -> Result<Self> {
        let mut frame = DataFrame::default();
        for spec in series {
            for row in load_series(spec)? {
                frame.insert(spec, row)?;
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

    /// The candle series for `symbol`, ascending by `time`.
    ///
    /// `open`/`high`/`low`/`close` are required; `volume` defaults to `0`.
    pub fn candles(&self, symbol: &str) -> Result<Vec<(String, Candle)>> {
        let mut out = Vec::new();
        for ((sym, time), row) in &self.rows {
            if sym != symbol {
                continue;
            }
            let field = |name: &str| -> Result<Real> {
                let raw = row
                    .get(name)
                    .ok_or_else(|| anyhow!("{symbol} @ {time}: missing required column `{name}`"))?;
                raw.parse::<Real>()
                    .with_context(|| format!("{symbol} @ {time}: column `{name}` = {raw:?}"))
            };
            let volume = match row.get("volume") {
                Some(raw) if !raw.is_empty() => raw
                    .parse::<Real>()
                    .with_context(|| format!("{symbol} @ {time}: column `volume` = {raw:?}"))?,
                _ => 0.0,
            };
            let candle = Candle::new(field("open")?, field("high")?, field("low")?, field("close")?, volume);
            out.push((time.clone(), candle));
        }
        if out.is_empty() {
            bail!("no rows found for symbol `{symbol}` across the given --series");
        }
        Ok(out)
    }
}

/// Expand one `--series` value into its rows.
///
/// `key=value` literals and `@file` loaders may appear in **any order** and in
/// **any number**, as long as there is at least one file. All files' rows are
/// concatenated and every literal is broadcast onto each of them.
fn load_series(spec: &str) -> Result<Vec<Row>> {
    let mut literals = Row::new();
    let mut files: Vec<&str> = Vec::new();

    for term in spec.split(',') {
        let term = term.trim();
        if term.is_empty() {
            continue;
        }
        if let Some(path) = term.strip_prefix('@') {
            files.push(path);
        } else if let Some((key, value)) = term.split_once('=') {
            let value = unquote(value.trim());
            // A literal value should never contain '@' — that means an `@file`
            // term got swallowed, usually because terms were joined with ';'
            // (the CSV delimiter) instead of ','.
            if value.contains('@') {
                bail!(
                    "series term `{term}`: a literal value can't contain '@'. Series terms are \
                     separated by ',' — e.g. \"symbol=AAPL,@candles.csv\""
                );
            }
            literals.insert(key.trim().to_lowercase(), value.to_string());
        } else {
            bail!("series term `{term}` is neither a `key=value` literal nor an `@file`");
        }
    }

    // Every series must load at least one CSV; literals only make sense broadcast
    // over a file's rows (and a literals-only row has no `time` to join on).
    if files.is_empty() {
        bail!(
            "series `{spec}` loads no CSV: every series needs at least one `@file.csv` term \
             (terms are separated by ',')"
        );
    }

    let mut rows = Vec::new();
    for path in files {
        for mut row in read_csv(path)? {
            row.extend(literals.iter().map(|(k, v)| (k.clone(), v.clone())));
            rows.push(row);
        }
    }
    Ok(rows)
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
        let frame = DataFrame::from_series(&[format!("symbol='BTC',@{path}")]).unwrap();
        let candles = frame.candles("BTC").unwrap();
        assert_eq!(candles.len(), 2);
        assert_eq!(candles[0].0, "1");
        assert_eq!(candles[0].1.close, 10.5);
        assert_eq!(candles[1].1.high, 12.0);
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
            format!("symbol=BTC,@{prices}"),
            format!("symbol=BTC,@{fundamentals}"),
        ])
        .unwrap();
        // The extra column rode along on the joined rows.
        assert_eq!(frame.rows[&("BTC".into(), "1".into())]["pe_ratio"], "15.0");
        // Candles still build (volume defaulted to 0).
        let candles = frame.candles("BTC").unwrap();
        assert_eq!(candles.len(), 2);
        assert_eq!(candles[0].1.volume, 0.0);
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
            DataFrame::from_series(&[format!("symbol=BTC,@{p1},exchange=NYSE,@{p2}")]).unwrap();
        // Both files' rows concatenated.
        assert_eq!(frame.candles("BTC").unwrap().len(), 4);
        // Both literals broadcast onto rows from either file.
        assert_eq!(frame.rows[&("BTC".into(), "1".into())]["exchange"], "NYSE");
        assert_eq!(frame.rows[&("BTC".into(), "4".into())]["exchange"], "NYSE");
    }
}
