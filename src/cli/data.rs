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

    /// Group the frame's rows by `(symbol, freq column value)` and return each
    /// group's `time` list in ascending order. The `freq` key is `None` for
    /// rows that carried no `freq` column, so a plain `time,open,high,low,close`
    /// CSV still surfaces as one group per symbol. Used by the calendar
    /// auto-detection ([`crate::calendar::detect_frequency`]) so different
    /// cadences of the same symbol aren't averaged into one median.
    pub fn series_times(&self) -> BTreeMap<(String, Option<String>), Vec<&str>> {
        let mut out: BTreeMap<(String, Option<String>), Vec<&str>> = BTreeMap::new();
        for ((sym, time), row) in &self.rows {
            let freq = row.get("freq").filter(|s| !s.is_empty()).cloned();
            out.entry((sym.clone(), freq)).or_default().push(time.as_str());
        }
        out
    }

    /// Return the ascending `time` list of the largest series matching
    /// `symbol`. When a symbol has multiple `freq` groups (see
    /// [`Self::series_times`]) — e.g. a 1d and a 1h stream in the same frame
    /// — the one with the most rows wins, since it is almost always the
    /// primary series the strategy is trading. Returns `None` when the frame
    /// carries nothing for `symbol`.
    pub fn dominant_series_times(&self, symbol: &str) -> Option<Vec<&str>> {
        self.series_times()
            .into_iter()
            .filter(|((sym, _), _)| sym == symbol)
            .max_by_key(|(_, times)| times.len())
            .map(|(_, times)| times)
    }

    /// The candle series for `symbol`, ascending by `time`.
    ///
    /// `open`/`high`/`low`/`close` are required; `volume` defaults to `0`.
    /// When the frame carries several `freq` groups for `symbol`, they are
    /// concatenated in `time` order — that's the single-run legacy shape.
    /// The batch driver ([`crate::batch`]) uses [`Self::groups`] instead, so
    /// each `(symbol, freq)` stays isolated.
    pub fn candles(&self, symbol: &str) -> Result<Vec<(String, Candle)>> {
        let mut out = Vec::new();
        for ((sym, time), row) in &self.rows {
            if sym != symbol {
                continue;
            }
            out.push((time.clone(), row_to_candle(sym, time, row)?));
        }
        if out.is_empty() {
            bail!("no rows found for symbol `{symbol}` across the given --series");
        }
        Ok(out)
    }

    /// Enumerate the frame's `(symbol, freq)` groups, one [`Group`] each,
    /// with the group's candles preassembled in ascending `time` order.
    /// This is the batch driver's iteration source — one `run_iteration` call per
    /// [`Group`]. Freq is `None` for rows that lacked a `freq` column, so a
    /// plain `time,open,high,low,close` CSV still surfaces as one group per
    /// symbol.
    pub fn groups(&self) -> Result<Vec<Group>> {
        type Bucket = Vec<(String, Candle)>;
        let mut buckets: BTreeMap<(String, Option<String>), Bucket> = BTreeMap::new();
        for ((sym, time), row) in &self.rows {
            let freq = row.get("freq").filter(|s| !s.is_empty()).cloned();
            let candle = row_to_candle(sym, time, row)?;
            buckets
                .entry((sym.clone(), freq))
                .or_default()
                .push((time.clone(), candle));
        }
        Ok(buckets
            .into_iter()
            .map(|((symbol, freq), candles)| Group {
                symbol,
                freq,
                candles,
            })
            .collect())
    }
}

/// One `(symbol, freq column value)` slice of the frame, ready to drive a
/// per-iteration backtest. Produced by [`DataFrame::groups`].
#[derive(Debug, Clone)]
pub struct Group {
    pub symbol: String,
    /// The value from the row's `freq` column, or `None` when the frame had
    /// no `freq` column (typical single-freq inputs).
    pub freq: Option<String>,
    /// Ascending by the row's `time` column.
    pub candles: Vec<(String, Candle)>,
}

/// Build a [`Candle`] from one row's OHLCV columns. Shared by both
/// [`DataFrame::candles`] and [`DataFrame::groups`].
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
            format!("symbol=BTC,@{prices}").parse().unwrap(),
            format!("symbol=BTC,@{fundamentals}").parse().unwrap(),
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
            DataFrame::from_series(&[format!("symbol=BTC,@{p1},exchange=NYSE,@{p2}").parse().unwrap()])
                .unwrap();
        // Both files' rows concatenated.
        assert_eq!(frame.candles("BTC").unwrap().len(), 4);
        // Both literals broadcast onto rows from either file.
        assert_eq!(frame.rows[&("BTC".into(), "1".into())]["exchange"], "NYSE");
        assert_eq!(frame.rows[&("BTC".into(), "4".into())]["exchange"], "NYSE");
    }

    #[test]
    fn series_without_a_file_is_rejected() {
        assert!("symbol=BTC".parse::<SeriesSpec>().is_err());
    }

    #[test]
    fn series_times_groups_by_symbol_and_freq() {
        // Same symbol, two freqs → two groups. Different symbols also split.
        let btc_1d = tmp_csv(
            "fugazi_series_times_btc_1d.csv",
            "time;freq;open;high;low;close\n2024-01-01;1d;10;11;9;10\n2024-01-02;1d;10;12;10;11\n",
        );
        let btc_1h = tmp_csv(
            "fugazi_series_times_btc_1h.csv",
            "time;freq;open;high;low;close\n2024-01-01T00:00:00Z;1h;10;11;9;10\n2024-01-01T01:00:00Z;1h;10;11;9;10\n",
        );
        let eth_1d = tmp_csv(
            "fugazi_series_times_eth_1d.csv",
            "time;freq;open;high;low;close\n2024-01-01;1d;20;21;19;20\n2024-01-02;1d;20;22;20;21\n",
        );
        let frame = DataFrame::from_series(&[
            format!("symbol=BTC,@{btc_1d}").parse().unwrap(),
            format!("symbol=BTC,@{btc_1h}").parse().unwrap(),
            format!("symbol=ETH,@{eth_1d}").parse().unwrap(),
        ])
        .unwrap();
        let groups = frame.series_times();
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[&("BTC".into(), Some("1d".into()))].len(), 2);
        assert_eq!(groups[&("BTC".into(), Some("1h".into()))].len(), 2);
        assert_eq!(groups[&("ETH".into(), Some("1d".into()))].len(), 2);
    }

    #[test]
    fn groups_yields_one_group_per_symbol_freq_pair() {
        let btc_1d = tmp_csv(
            "fugazi_groups_btc_1d.csv",
            "time;freq;open;high;low;close\n2024-01-01;1d;10;11;9;10\n2024-01-02;1d;10;12;10;11\n",
        );
        let btc_1h = tmp_csv(
            "fugazi_groups_btc_1h.csv",
            "time;freq;open;high;low;close\n2024-01-01T00:00:00Z;1h;10;11;9;10\n2024-01-01T01:00:00Z;1h;10;11;9;10\n",
        );
        let eth_1d = tmp_csv(
            "fugazi_groups_eth_1d.csv",
            "time;freq;open;high;low;close\n2024-01-01;1d;20;21;19;20\n2024-01-02;1d;20;22;20;21\n",
        );
        let frame = DataFrame::from_series(&[
            format!("symbol=BTC,@{btc_1d}").parse().unwrap(),
            format!("symbol=BTC,@{btc_1h}").parse().unwrap(),
            format!("symbol=ETH,@{eth_1d}").parse().unwrap(),
        ])
        .unwrap();
        let groups = frame.groups().unwrap();
        assert_eq!(groups.len(), 3);
        let by_key: std::collections::HashMap<_, _> = groups
            .iter()
            .map(|g| ((g.symbol.clone(), g.freq.clone()), g.candles.len()))
            .collect();
        assert_eq!(by_key[&("BTC".into(), Some("1d".into()))], 2);
        assert_eq!(by_key[&("BTC".into(), Some("1h".into()))], 2);
        assert_eq!(by_key[&("ETH".into(), Some("1d".into()))], 2);
    }

    #[test]
    fn groups_defaults_missing_freq_to_none() {
        let path = tmp_csv(
            "fugazi_groups_nofreq.csv",
            "time;open;high;low;close\n2024-01-01;10;11;9;10\n2024-01-02;10;12;10;11\n",
        );
        let frame = DataFrame::from_series(&[format!("symbol=BTC,@{path}").parse().unwrap()]).unwrap();
        let groups = frame.groups().unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].symbol, "BTC");
        assert_eq!(groups[0].freq, None);
        assert_eq!(groups[0].candles.len(), 2);
    }

    #[test]
    fn series_times_defaults_missing_freq_to_none() {
        // No `freq` column at all → the group key's freq slot is None.
        let path = tmp_csv(
            "fugazi_series_times_nofreq.csv",
            "time;open;high;low;close\n2024-01-01;10;11;9;10\n2024-01-02;10;12;10;11\n",
        );
        let frame = DataFrame::from_series(&[format!("symbol=BTC,@{path}").parse().unwrap()]).unwrap();
        let groups = frame.series_times();
        assert_eq!(groups.len(), 1);
        assert!(groups.contains_key(&("BTC".into(), None)));
    }
}
