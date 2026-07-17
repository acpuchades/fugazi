//! Typed CLI inputs that follow fugazi's `@file` convention.
//!
//! Several flags take content that may be given either as a file to load or
//! literally on the command line. [`Source`] captures that choice once — a clap
//! value parser (via [`FromStr`]) turns `@path` into [`Source::File`] and anything
//! else into [`Source::Inline`], so the rest of the CLI works with a decided type
//! instead of re-detecting the `@` prefix at every use site.

use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Context, Result, anyhow};

/// Parse `text` (YAML) into a [`serde_json::Value`] — the common shape the spec
/// and params loaders both work on. The document is normalized via
/// [`crate::convert::yaml_to_json`] so `!tags` become serde_json's singleton-map
/// external-tag form. (JSON is a subset of YAML, so JSON-shaped text still parses.)
///
/// Errors carry only the inner YAML message (with the serde_norway `, line: N,
/// column: M` suffix); callers that know a filename should reach for
/// [`parse_value_at`] instead so the origin lands in the message. Kept for the
/// spec / imports / template tests that pass literal YAML with no meaningful
/// origin — the bin never calls it, which is why the `dead_code` allow is here.
#[allow(dead_code)]
pub fn parse_value(text: &str) -> Result<serde_json::Value> {
    parse_value_at(text, "(input)")
}

/// [`parse_value`] with a source label — a file path, `(inline)`, or another
/// short origin string — folded into the error prefix as `<label>:<line>:<col>:`
/// when the underlying YAML parse reports a location.
pub fn parse_value_at(text: &str, label: &str) -> Result<serde_json::Value> {
    let value: serde_norway::Value = serde_norway::from_str(text)
        .map_err(|e| yaml_parse_error(e, label))?;
    crate::convert::yaml_to_json(value)
        .with_context(|| format!("normalising YAML tags in {label}"))
}

/// Prefix a serde_norway parse error with the source label and (when the parser
/// reports one) the offending line/column. The raw error already trails a
/// `, line: N, column: M`, so a caller reading the full chain sees the location
/// once at the front and once in the body — the front prefix is what makes it
/// grep-friendly.
fn yaml_parse_error(err: serde_norway::Error, label: &str) -> anyhow::Error {
    match err.location() {
        Some(loc) => anyhow!("{label}:{}:{}: {err}", loc.line(), loc.column()),
        None => anyhow!("{label}: {err}"),
    }
}

/// A text input given as either `@path` (load the file) or inline content
/// (anything else) — the same `@` convention `--series` uses for its CSVs.
#[derive(Debug, Clone)]
pub enum Source {
    /// `@path`: read the content from this file.
    File(PathBuf),
    /// Anything else: the content itself.
    Inline(String),
}

impl Source {
    /// The content: the file's text for [`File`](Self::File), the literal for
    /// [`Inline`](Self::Inline).
    pub fn read(&self) -> Result<String> {
        match self {
            Source::File(path) => std::fs::read_to_string(path)
                .with_context(|| format!("reading file `{}`", path.display())),
            Source::Inline(text) => Ok(text.clone()),
        }
    }

    /// A short label for logs: the path for a file, `(inline)` for inline content.
    pub fn label(&self) -> String {
        match self {
            Source::File(path) => path.display().to_string(),
            Source::Inline(_) => "(inline)".to_string(),
        }
    }

    /// The directory a relative `!import` path inside this input resolves
    /// against: the file's own directory for `@path` (so a strategy's imports
    /// are relative to the strategy, not to wherever `fugazi` was invoked
    /// from), and the working directory for inline text, which has no
    /// directory of its own. See [`crate::imports`].
    pub fn base_dir(&self) -> PathBuf {
        match self {
            Source::File(path) => match path.parent() {
                Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
                _ => PathBuf::from("."),
            },
            Source::Inline(_) => PathBuf::from("."),
        }
    }

    /// If this is inline content that resembles an old-style bare file path
    /// (single line ending in `.yml`/`.yaml`), the would-be path — used to hint at
    /// the `@` form when such a value fails to parse.
    pub fn misused_path(&self) -> Option<&str> {
        match self {
            Source::Inline(text)
                if !text.contains('\n')
                    && (text.ends_with(".yml") || text.ends_with(".yaml")) =>
            {
                Some(text)
            }
            _ => None,
        }
    }
}

impl FromStr for Source {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.strip_prefix('@') {
            Some(path) => Source::File(PathBuf::from(path)),
            None => Source::Inline(s.to_string()),
        })
    }
}

/// Which strategy shape a [`StrategySource`] resolves to.
///
/// The default (no prefix, or `single:`) is a
/// [`SingleAssetStrategy`](fugazi::strategies::SingleAssetStrategy). Prefixing
/// with `pairs:` (e.g. `pairs:@spread.yml`) declares a two-symbol pair-trading
/// spec that resolves to a
/// [`PairsStrategy`](fugazi::strategies::PairsStrategy). Prefixing with
/// `basket:` (e.g. `basket:@basket.yml`) declares an N-symbol cross-sectional
/// basket that resolves to a
/// [`BasketStrategy`](fugazi::strategies::BasketStrategy). Prefixing with
/// `multi:` (e.g. `multi:@portfolio.yml`) declares an N-symbol per-asset
/// independent strategy — every symbol runs the same signals in isolation —
/// that resolves to a
/// [`MultiAssetStrategy`](fugazi::strategies::MultiAssetStrategy). Prefixing
/// with `portfolio:` (e.g. `portfolio:@portfolio.yml`) declares a composite
/// N-child portfolio — a heterogeneous mix of single / pairs / basket /
/// multi strategies sharing one cash pool via per-child sub-wallets — that
/// resolves to a [`Portfolio`](fugazi::portfolio::Portfolio).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrategyKind {
    /// A single-asset strategy (default; matches `@file.yml` and `single:@file.yml`).
    Single,
    /// A two-symbol pair-trading strategy (`pairs:@file.yml`).
    Pairs,
    /// An N-symbol cross-sectional basket strategy (`basket:@file.yml`).
    Basket,
    /// An N-symbol independent multi-asset strategy (`multi:@file.yml`).
    Multi,
    /// A composite portfolio over N heterogeneous child strategies
    /// (`portfolio:@file.yml`).
    Portfolio,
}

/// A strategy positional: a [`Source`] plus a decided [`StrategyKind`] from
/// the optional leading shape prefix. `single:` and no-prefix both resolve to
/// [`StrategyKind::Single`]; `pairs:` resolves to [`StrategyKind::Pairs`];
/// `basket:` resolves to [`StrategyKind::Basket`].
#[derive(Debug, Clone)]
pub struct StrategySource {
    pub kind: StrategyKind,
    pub source: Source,
}

impl StrategySource {
    pub fn read(&self) -> anyhow::Result<String> {
        self.source.read()
    }

    pub fn label(&self) -> String {
        self.source.label()
    }

    /// The directory this strategy's `!import` paths resolve against — see
    /// [`Source::base_dir`].
    pub fn base_dir(&self) -> PathBuf {
        self.source.base_dir()
    }

    pub fn misused_path(&self) -> Option<&str> {
        self.source.misused_path()
    }
}

impl FromStr for StrategySource {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some(rest) = s.strip_prefix("single:") {
            return Ok(StrategySource {
                kind: StrategyKind::Single,
                source: rest.parse().expect("infallible"),
            });
        }
        if let Some(rest) = s.strip_prefix("pairs:") {
            return Ok(StrategySource {
                kind: StrategyKind::Pairs,
                source: rest.parse().expect("infallible"),
            });
        }
        if let Some(rest) = s.strip_prefix("basket:") {
            return Ok(StrategySource {
                kind: StrategyKind::Basket,
                source: rest.parse().expect("infallible"),
            });
        }
        if let Some(rest) = s.strip_prefix("multi:") {
            return Ok(StrategySource {
                kind: StrategyKind::Multi,
                source: rest.parse().expect("infallible"),
            });
        }
        if let Some(rest) = s.strip_prefix("portfolio:") {
            return Ok(StrategySource {
                kind: StrategyKind::Portfolio,
                source: rest.parse().expect("infallible"),
            });
        }
        Ok(StrategySource {
            kind: StrategyKind::Single,
            source: s.parse().expect("infallible"),
        })
    }
}
