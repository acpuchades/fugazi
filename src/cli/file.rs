//! `file:PATH` provider for `fugazi get` — reads OHLCV bars from a local CSV
//! (typically one previously produced by `fugazi get`). The file's `symbol`,
//! `freq`, `time`, `open`, `high`, `low`, `close` and `volume` columns become
//! the bars; extra columns (e.g. overlays from a previous run) are ignored.
//! Delimiter is autodetected from the header (`;`, `,`, `\t`, `|`) — the same
//! rule `--series` follows.
//!
//! Unlike the remote providers, `file:` doesn't fit the standard
//! `provider:SYMBOL[freq]` spec grammar (the file already carries symbol+freq
//! per row), so `get.rs` special-cases the `file:` prefix: after the colon the
//! whole remainder is the path, and enumeration of the file's own
//! `(symbol, interval)` combinations drives the per-series pipeline.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use fugazi::prelude::*;
use fugazi::sources::{Interval, Timestamp};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::get::parse_interval;

/// A local CSV file of OHLCV bars, in the shape [`fugazi get`] itself writes.
pub struct FileSource {
    path: PathBuf,
}

/// One row from the file, with `symbol` and `freq` promoted out of the CSV's
/// columns so the caller can group / filter without re-parsing.
#[derive(Debug, Clone)]
pub struct FileBar {
    pub symbol: String,
    pub interval: Interval,
    pub time: Timestamp,
    pub candle: Candle,
}

impl FileSource {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Read every OHLCV row in the file. `symbol`, `freq`, `time`, `open`,
    /// `high`, `low`, `close` are required columns; `volume` defaults to `0`
    /// when missing or blank.
    pub fn read(&self) -> Result<Vec<FileBar>> {
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

        let mut out = Vec::new();
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
            let time = parse_time(field(i_time))
                .with_context(|| format!("{path}: row {line}: column `time`"))?;
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
            out.push(FileBar {
                symbol,
                interval,
                time,
                candle,
            });
        }
        Ok(out)
    }
}

/// RFC3339 first (what `fugazi get` writes), then a raw millisecond epoch
/// (matches [`Timestamp`]'s native ABI). Two forms — enough to cover the
/// common shapes without inventing a new grammar.
fn parse_time(s: &str) -> Result<Timestamp> {
    let s = s.trim();
    if let Ok(dt) = OffsetDateTime::parse(s, &Rfc3339) {
        return Ok(Timestamp::from_datetime(dt));
    }
    if let Ok(ms) = s.parse::<i64>() {
        return Ok(Timestamp(ms));
    }
    Err(anyhow!(
        "expected RFC3339 timestamp or millisecond epoch, got {s:?}"
    ))
}

/// Guess a CSV's column delimiter from its header line: whichever of `; , \t |`
/// occurs most often wins (ties favour earlier in that list); a single-column
/// file with none of them falls back to `,`. Mirrors [`crate::data`]'s rule so
/// both `--series` and `file:` read the same files identically.
fn detect_delimiter(path: &Path) -> Result<u8> {
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
            "fugazi_file_source_test_a.csv",
            "symbol;freq;time;open;high;low;close;volume\n\
             BTCUSDT;1d;2024-01-01T00:00:00Z;100;110;90;105;1000\n\
             BTCUSDT;1d;2024-01-02T00:00:00Z;105;115;100;108;1200\n",
        );
        let bars = FileSource::new(path).read().unwrap();
        assert_eq!(bars.len(), 2);
        assert_eq!(bars[0].symbol, "BTCUSDT");
        assert_eq!(bars[0].interval, Interval::Day(1));
        assert_eq!(bars[0].candle.close, 105.0);
        assert_eq!(bars[0].time, Timestamp(1_704_067_200_000));
        assert_eq!(bars[1].candle.volume, 1200.0);
    }

    #[test]
    fn autodetects_comma_delimiter() {
        let path = tmp_csv(
            "fugazi_file_source_test_b.csv",
            "symbol,freq,time,open,high,low,close,volume\n\
             ETHUSDT,1h,2024-01-01T00:00:00Z,10,11,9,10.5,50\n",
        );
        let bars = FileSource::new(path).read().unwrap();
        assert_eq!(bars.len(), 1);
        assert_eq!(bars[0].interval, Interval::Hour(1));
    }

    #[test]
    fn tolerates_missing_volume_column_and_blank_volume_cell() {
        let path = tmp_csv(
            "fugazi_file_source_test_c.csv",
            "symbol;freq;time;open;high;low;close\n\
             AAA;1d;2024-01-01T00:00:00Z;1;2;0.5;1.5\n",
        );
        let bars = FileSource::new(path).read().unwrap();
        assert_eq!(bars[0].candle.volume, 0.0);

        let path = tmp_csv(
            "fugazi_file_source_test_d.csv",
            "symbol;freq;time;open;high;low;close;volume\n\
             AAA;1d;2024-01-01T00:00:00Z;1;2;0.5;1.5;\n",
        );
        let bars = FileSource::new(path).read().unwrap();
        assert_eq!(bars[0].candle.volume, 0.0);
    }

    #[test]
    fn accepts_millisecond_epoch_time() {
        let path = tmp_csv(
            "fugazi_file_source_test_e.csv",
            "symbol;freq;time;open;high;low;close;volume\n\
             AAA;1d;1704067200000;1;2;0.5;1.5;10\n",
        );
        let bars = FileSource::new(path).read().unwrap();
        assert_eq!(bars[0].time, Timestamp(1_704_067_200_000));
    }

    #[test]
    fn ignores_extra_overlay_columns() {
        let path = tmp_csv(
            "fugazi_file_source_test_f.csv",
            "symbol;freq;time;open;high;low;close;volume;sma20;ema50\n\
             AAA;1d;2024-01-01T00:00:00Z;1;2;0.5;1.5;10;1.4;1.3\n",
        );
        let bars = FileSource::new(path).read().unwrap();
        assert_eq!(bars.len(), 1);
        assert_eq!(bars[0].candle.close, 1.5);
    }

    #[test]
    fn rejects_missing_required_column() {
        let path = tmp_csv(
            "fugazi_file_source_test_g.csv",
            "symbol;time;open;high;low;close;volume\n",
        );
        let err = FileSource::new(path).read().unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("missing required column `freq`"), "{msg}");
    }

    #[test]
    fn rejects_bad_freq_token() {
        let path = tmp_csv(
            "fugazi_file_source_test_h.csv",
            "symbol;freq;time;open;high;low;close;volume\n\
             AAA;1x;2024-01-01T00:00:00Z;1;2;0.5;1.5;10\n",
        );
        let err = FileSource::new(path).read().unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("column `freq`"), "{msg}");
    }
}
