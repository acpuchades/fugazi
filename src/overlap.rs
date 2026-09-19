//! How much of a multi-symbol universe ever occupies the **same** snapshot.
//!
//! Bars are grouped into snapshots by **exact timestamp** everywhere in this
//! crate — the CLI's `get` as a dataset is assembled, its `run` as one is read
//! back, and any caller building [`Snapshot`](crate::types::Snapshot)s by
//! hand. That exactness is deliberate: Tokyo closes before New York opens, so
//! folding a `^N225` bar and an `SPY` bar into one snapshot because they share
//! a *date* would hand a strategy trading `^N225` an S&P close from thirteen
//! hours in its future. Exact stamps are what stop this crate manufacturing
//! lookahead across time zones, and nothing here relaxes them.
//!
//! The cost is a failure mode that is silent by construction. A universe
//! assembled from differently-timed sessions can fragment completely — nine
//! index symbols stamped at five session opens produce snapshots holding at
//! most four of them — and every surface still looks right: each symbol has its
//! full history, the row and bar counts are correct, and only the *joint*
//! occupancy is wrong. Downstream it surfaces as a cross-sectional strategy
//! ranking a smaller universe than the one declared, or as a `!pick` across the
//! boundary resolving to nothing and yielding an all-empty column —
//! indistinguishable, in a CSV, from an indicator still warming up.
//!
//! [`measure`] is the one pass, over timestamps the caller already holds; the
//! CLI's `overlap::warn_if_fragmented` is its console report, and the Python
//! bindings surface the same measurement as `measure_overlap`. The module
//! lives in the library — not the binary — precisely because two of the
//! crate's three consumers build their own snapshot streams.
//!
//! **Observed co-occurrence, never per-symbol session signatures.** Daylight
//! saving alone gives `^FTSE` `{07:00, 08:00}` against `^GDAXI`
//! `{06:00, 07:00, 08:00}` — different signatures for series that share plenty
//! of bars. Nothing here compares stamp *sets*; it counts what actually landed
//! together.

use std::collections::{BTreeMap, BTreeSet};

/// What the widest snapshot of an assembled universe actually held.
///
/// `K` is whatever the caller groups snapshots by — the same key its driver
/// uses, so the figure describes the real thing rather than a re-derivation of
/// it: UTC millis for the CLI's `get`, the CSV's own time label for its `run`
/// (which joins on that string), the snapshot index for a pre-built stream.
#[derive(Debug, PartialEq, Eq)]
pub struct Overlap<K> {
    /// Distinct symbols across every keyed sample.
    pub total: usize,
    /// The most symbols any single snapshot held — the widest view a strategy
    /// reading this universe will ever get.
    pub widest: usize,
    /// That snapshot's members, sorted. Ties go to the earliest key.
    pub widest_symbols: Vec<String>,
    /// Which snapshot it was.
    pub at: Option<K>,
    /// Symbols that never shared a snapshot with *any* other symbol.
    pub isolated: Vec<String>,
    /// Distinct snapshots, and how many held exactly one symbol.
    pub snapshots: usize,
    pub singletons: usize,
}

// Hand-written rather than derived: `#[derive(Default)]` would demand
// `K: Default`, which `at: Option<K>` does not need.
impl<K> Default for Overlap<K> {
    fn default() -> Self {
        Self {
            total: 0,
            widest: 0,
            widest_symbols: Vec::new(),
            at: None,
            isolated: Vec::new(),
            snapshots: 0,
            singletons: 0,
        }
    }
}

impl<K> Overlap<K> {
    /// Whether no snapshot anywhere held the whole universe. False for a
    /// single-symbol stream, which has nothing to co-occur with.
    pub fn is_fragmented(&self) -> bool {
        self.total > 1 && self.widest < self.total
    }

    /// The summary-block value: `widest snapshot: N of M symbols`. Callers
    /// print it only when `total > 1` — for one symbol it says nothing.
    pub fn summary(&self) -> String {
        format!("widest snapshot: {} of {} symbols", self.widest, self.total)
    }
}

/// Measure co-occurrence over `(snapshot key, symbol)` pairs — one pass, using
/// timestamps the caller already has in hand.
///
/// Samples the caller cannot key are simply not passed in: an atom with no
/// parsed time can't be aligned with anything, and counting it would report a
/// fragmentation that is really a missing-`time` problem.
///
/// The map is ordered, so the widest snapshot's tie-break — earliest key wins —
/// falls out of the iteration order rather than a comparison, and the reported
/// figure is stable across runs over the same input.
pub fn measure<'a, K: Ord>(pairs: impl IntoIterator<Item = (K, &'a str)>) -> Overlap<K> {
    let mut by_key: BTreeMap<K, BTreeSet<&'a str>> = BTreeMap::new();
    for (key, symbol) in pairs {
        by_key.entry(key).or_default().insert(symbol);
    }

    let mut all: BTreeSet<&str> = BTreeSet::new();
    let mut paired: BTreeSet<&str> = BTreeSet::new();
    let mut singletons = 0usize;
    let mut widest = 0usize;
    let mut widest_symbols: Vec<String> = Vec::new();
    let mut at: Option<K> = None;
    let snapshots = by_key.len();

    for (key, syms) in by_key {
        all.extend(syms.iter().copied());
        if syms.len() == 1 {
            singletons += 1;
        } else {
            paired.extend(syms.iter().copied());
        }
        // Strictly greater: ascending iteration means the first snapshot to
        // reach a given width keeps the slot.
        if syms.len() > widest {
            widest = syms.len();
            widest_symbols = syms.iter().map(|s| (*s).to_string()).collect();
            at = Some(key);
        }
    }

    Overlap {
        total: all.len(),
        widest,
        widest_symbols,
        at,
        isolated: all.difference(&paired).map(|s| (*s).to_string()).collect(),
        snapshots,
        singletons,
    }
}

/// Measure over the per-symbol atom streams the universe drivers build —
/// `(symbol, [(time label, atom)])`, the shape the CLI's
/// `run::join_universe_by_time` consumes in `run` and `optimize` alike.
///
/// Keyed on the **time label**, not the parsed `Atom::time`: the label is what
/// the joiner groups on, so this measures the snapshots the strategy will
/// actually see. A label that doesn't parse as a date leaves `Atom::time`
/// `None` but still joins fine, and keying on the parse would quietly measure
/// nothing.
///
/// Generic in the atom type so this module stays free of the market vocabulary;
/// only the label and the symbol are read.
pub fn measure_universe<S: AsRef<str>, K: std::fmt::Display, T>(
    per_symbol: &[(S, Vec<(K, T)>)],
) -> Overlap<String> {
    measure(per_symbol.iter().flat_map(|(sym, atoms)| {
        atoms
            .iter()
            .map(move |(key, _)| (key.to_string(), sym.as_ref()))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(stamp, symbol)` pairs from a compact `(stamp, [symbols])` table.
    fn pairs<'a>(table: &'a [(&'a str, &'a [&'a str])]) -> Vec<(&'a str, &'a str)> {
        table
            .iter()
            .flat_map(|(t, syms)| syms.iter().map(move |s| (*t, *s)))
            .collect()
    }

    /// The macro-index case: nine symbols across five session opens, never more
    /// than four of them on one stamp.
    #[test]
    fn a_fragmented_universe_reports_its_widest_snapshot() {
        let o = measure(pairs(&[
            ("2024-01-02T00:00Z", &["^N225"]),
            ("2024-01-02T01:30Z", &["^HSI"]),
            ("2024-01-02T07:00Z", &["^FTSE", "^GDAXI"]),
            ("2024-01-02T13:00Z", &["^BVSP"]),
            ("2024-01-02T13:30Z", &["SPY", "^GSPC", "^NDX", "EEM"]),
        ]));

        assert!(o.is_fragmented());
        assert_eq!(o.total, 9);
        assert_eq!(o.widest, 4);
        assert_eq!(o.widest_symbols, ["EEM", "SPY", "^GSPC", "^NDX"]);
        assert_eq!(o.at, Some("2024-01-02T13:30Z"));
        // ^FTSE/^GDAXI share 07:00 with each other, so only the three lone
        // sessions are isolated.
        assert_eq!(o.isolated, ["^BVSP", "^HSI", "^N225"]);
        assert_eq!((o.snapshots, o.singletons), (5, 3));
        assert_eq!(o.summary(), "widest snapshot: 4 of 9 symbols");
    }

    /// Observed co-occurrence, not stamp signatures: daylight saving gives
    /// these two different stamp sets for series that share every bar that
    /// matters.
    #[test]
    fn daylight_saving_alone_is_not_fragmentation() {
        // ^FTSE {07:00, 08:00} vs ^GDAXI {06:00, 07:00, 08:00}.
        let o = measure(pairs(&[
            ("2024-01-02T06:00Z", &["^GDAXI"]),
            ("2024-01-03T07:00Z", &["^FTSE", "^GDAXI"]),
            ("2024-01-04T08:00Z", &["^FTSE", "^GDAXI"]),
        ]));
        assert!(!o.is_fragmented());
        assert_eq!((o.total, o.widest), (2, 2));
        assert!(o.isolated.is_empty());
    }

    /// A universe that meets on *some* bars is ordinary — a listing gap, a
    /// holiday, a half-day. `widest == total`, so nothing fires even though a
    /// snapshot is partial.
    #[test]
    fn a_listing_gap_is_not_fragmentation() {
        let o = measure(pairs(&[
            ("2024-01-02T14:30Z", &["AAPL", "MSFT"]),
            ("2024-01-03T14:30Z", &["AAPL"]),
        ]));
        assert!(!o.is_fragmented());
        assert_eq!((o.total, o.widest, o.singletons), (2, 2, 1));
    }

    /// Ties go to the earliest key, so the reported snapshot doesn't wander
    /// between runs over the same input.
    #[test]
    fn the_widest_snapshot_tie_breaks_on_the_earliest_key() {
        let o = measure(pairs(&[
            ("2024-01-02T13:30Z", &["A", "B"]),
            ("2024-01-02T14:30Z", &["A", "B"]),
        ]));
        assert_eq!(o.at, Some("2024-01-02T13:30Z"));
    }

    /// One symbol has nothing to co-occur with, however its stream is shaped.
    #[test]
    fn a_single_symbol_universe_is_never_fragmented() {
        let o = measure(pairs(&[
            ("2024-01-02T13:30Z", &["SPY"]),
            ("2024-01-03T13:30Z", &["SPY"]),
        ]));
        assert!(!o.is_fragmented());
        assert_eq!((o.total, o.widest), (1, 1));
    }

    /// Every symbol alone on its own stamp — the fully-degenerate universe.
    #[test]
    fn a_universe_where_nothing_ever_meets() {
        let o = measure(pairs(&[
            ("2024-01-02T00:00Z", &["A"]),
            ("2024-01-02T01:00Z", &["B"]),
            ("2024-01-02T02:00Z", &["C"]),
        ]));
        assert!(o.is_fragmented());
        assert_eq!((o.total, o.widest), (3, 1));
        assert_eq!(o.isolated, ["A", "B", "C"]);
        assert_eq!(o.widest_symbols, ["A"]);
    }

    #[test]
    fn an_empty_stream_measures_to_nothing() {
        let o: Overlap<&str> = measure(std::iter::empty());
        assert_eq!(o, Overlap::default());
        assert!(!o.is_fragmented());
    }
}
