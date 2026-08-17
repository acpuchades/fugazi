//! Loading the committed CSVs under `tests/data/`, and the policy for what
//! happens when a *generated* one is missing.
//!
//! # The skip-vs-fail policy
//!
//! Four cross-validation suites (`talib_validation.rs`, `metrics_validation.rs`,
//! `wallet_validation.rs`, `trade_metrics_validation.rs`) compare fugazi against
//! an external reference library. None of those libraries is a Cargo
//! dependency, so the fixtures they consume are produced once by
//! `tools/gen_*.py` and committed. When a fixture is absent the suite **skips**,
//! so `cargo test` stays green for a contributor who has none of them
//! installed.
//!
//! That is a reasonable default and a terrible guarantee: a skip is
//! indistinguishable from a pass in CI, so a cross-check can rot for months
//! without a single red build. `docs/CONTRIBUTING.md` lists
//! `tests/talib_validation.rs` as a **drift guard**; a drift guard that
//! silently disables itself guards nothing.
//!
//! So the skip is opt-out. Setting `FUGAZI_REQUIRE_FIXTURES=1` turns every
//! missing-or-stale fixture from a skip into a failure — that is the mode a CI
//! job that *does* provision the reference libraries should run in, and the way
//! to prove locally that a suite is really comparing rather than returning
//! early. [`require_fixtures`] reads the switch; [`skip`] honours it.
//!
//! One hole the switch does not cover: it fires on a fixture that went
//! *missing*, not on a metric that was never written into one. That is
//! `tests/metrics_coverage.rs`'s job — it reads these fixtures for their key
//! sets alone, so it needs no reference library and deliberately never skips.

use std::path::PathBuf;

/// Absolute path to a file under `tests/data/`.
pub fn data_path(name: &str) -> PathBuf {
    [env!("CARGO_MANIFEST_DIR"), "tests", "data", name]
        .iter()
        .collect()
}

/// Whether a missing or stale generated fixture must fail rather than skip.
///
/// Set `FUGAZI_REQUIRE_FIXTURES=1` (any non-empty value other than `0`).
pub fn require_fixtures() -> bool {
    match std::env::var("FUGAZI_REQUIRE_FIXTURES") {
        Ok(v) => !v.is_empty() && v != "0",
        Err(_) => false,
    }
}

/// Report a cross-check as skipped — or fail it, under [`require_fixtures`].
///
/// `reason` says what is missing, `hint` how to regenerate it. Returns so the
/// caller can `return` out of the test; it only ever returns in skip mode.
#[track_caller]
pub fn skip(suite: &str, reason: &str, hint: &str) {
    assert!(
        !require_fixtures(),
        "{suite}: {reason}\n\
         FUGAZI_REQUIRE_FIXTURES is set, so this is a failure rather than a skip.\n\
         Regenerate the fixture:\n{hint}"
    );
    eprintln!(
        "\n\
         ==================================================================\n\
         SKIP {suite}: {reason}\n\
         This cross-check did NOT run. To regenerate the fixture:\n\
         {hint}\n\
         Set FUGAZI_REQUIRE_FIXTURES=1 to make this a failure instead.\n\
         ==================================================================\n"
    );
}

/// A parsed CSV: header row plus data rows, split on `,` with no quoting or
/// escaping — every fixture under `tests/data/` is plain numeric CSV.
pub struct Csv {
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

impl Csv {
    /// Read `tests/data/<name>`, or `None` when it isn't there.
    pub fn load(name: &str) -> Option<Self> {
        let text = std::fs::read_to_string(data_path(name)).ok()?;
        let mut lines = text.lines();
        let headers = lines
            .next()?
            .split(',')
            .map(|s| s.trim().to_string())
            .collect();
        let rows = lines
            .filter(|l| !l.trim().is_empty())
            .map(|l| l.split(',').map(|s| s.trim().to_string()).collect())
            .collect();
        Some(Self { headers, rows })
    }

    /// Read a fixture that must exist — a *committed input*, not a generated
    /// reference. Its absence is a broken checkout, not a missing tool.
    #[track_caller]
    pub fn require(name: &str) -> Self {
        Self::load(name)
            .unwrap_or_else(|| panic!("missing committed fixture tests/data/{name}"))
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn has(&self, column: &str) -> bool {
        self.headers.iter().any(|h| h == column)
    }

    /// The first of `columns` this CSV is missing, if any — the staleness probe
    /// for a generated fixture that predates a newly added column.
    pub fn missing<'a>(&self, columns: &[&'a str]) -> Option<&'a str> {
        columns.iter().copied().find(|c| !self.has(c))
    }

    #[track_caller]
    fn index_of(&self, column: &str) -> usize {
        self.headers
            .iter()
            .position(|h| h == column)
            .unwrap_or_else(|| {
                panic!("no column `{column}`; have {:?}", self.headers)
            })
    }

    /// A column read verbatim, without parsing.
    ///
    /// For the `(metric, expected)` fixtures, whose key column is the metric's
    /// dotted path rather than a number. Pair it with [`floats`](Self::floats)
    /// on the value column.
    #[track_caller]
    pub fn strings(&self, column: &str) -> Vec<String> {
        let idx = self.index_of(column);
        self.rows.iter().map(|row| row[idx].clone()).collect()
    }

    /// A fully-populated numeric column.
    #[track_caller]
    pub fn floats(&self, column: &str) -> Vec<f64> {
        let idx = self.index_of(column);
        self.rows
            .iter()
            .enumerate()
            .map(|(r, row)| {
                row[idx]
                    .parse()
                    .unwrap_or_else(|e| panic!("{column}[{r}] = `{}`: {e}", row[idx]))
            })
            .collect()
    }

    /// A numeric column where an empty cell means "the reference had no value
    /// here" (warm-up, or a NaN the generator wrote blank).
    #[track_caller]
    pub fn optional_floats(&self, column: &str) -> Vec<Option<f64>> {
        let idx = self.index_of(column);
        self.rows
            .iter()
            .enumerate()
            .map(|(r, row)| {
                let cell = &row[idx];
                (!cell.is_empty()).then(|| {
                    cell.parse()
                        .unwrap_or_else(|e| panic!("{column}[{r}] = `{cell}`: {e}"))
                })
            })
            .collect()
    }
}
