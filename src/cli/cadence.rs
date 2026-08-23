//! Which cadence a run is actually targeting, and whether the input agrees.
//!
//! A `--series` frame is a pile of rows. Nothing in it says "this backtest is a
//! daily backtest" — the cadence is inferred, per symbol, from the median gap
//! between timestamps ([`calendar::detect_frequency_from_atoms`]), and then used
//! for annualization, for `-w`'s duration form, for `trading_seconds_per_bar`,
//! and for matching freq-scoped `--costs` entries. Until this module existed
//! that inference was made **once, off one representative symbol**, and applied
//! to the whole universe with nothing checking that the universe agreed.
//!
//! Three ways that goes wrong, all of them silent:
//!
//! 1. **One symbol, two cadences.** `fugazi get binance:BTCUSDT[1d,1h]` writes
//!    both into one file. Read back, they are one symbol with two interleaved
//!    bar streams; no strategy can trade both, and detection reports whichever
//!    cadence dominates the row count.
//! 2. **A universe that disagrees.** Half the symbols daily, half hourly.
//!    Annualization takes the representative's answer, so every Sharpe in the
//!    report is scaled by a factor that is right for one series and wrong for
//!    the others.
//! 3. **A label that lies.** A file whose `freq` column says `1d` but whose
//!    stamps are an hour apart. The declared cadence is what freq-scoped costs
//!    match on; the detected one is what the calendar uses. They disagree, and
//!    neither surface says so.
//!
//! So the frame is censused once, at load, before anything reads it — the same
//! bargain [`crate::overlap`] strikes for snapshot co-occurrence: measure from
//! data already in hand, report in the terms of the command in front of the
//! user, and refuse only where refusing is the safe default.
//!
//! # What is an error and what is a warning
//!
//! Ambiguity is an **error**: a symbol carrying two cadences cannot be traded,
//! and picking one for the user would be a guess with a plausible-looking
//! result. `-f/--frequency SYM:CODE` is how the user resolves it, and the same
//! flag naming a cadence the frame does not carry is a typo, also an error.
//!
//! Disagreement is a **warning**: a mixed-cadence universe is measurable, just
//! not annualizable by one factor, and a mislabelled series still runs. Both say
//! the number about to be printed means something other than it appears to, so
//! they go to stderr regardless of `--quiet` — which governs a command's success
//! summary, not a finding about its data.
//!
//! # Precedence
//!
//! `-f/--frequency` (symbol-scoped beats unscoped) → the `freq` **column** →
//! the cadence **detected** from timestamp gaps. The middle term is the one this
//! module adds: a provider that told us the cadence outranks arithmetic on the
//! gaps between the bars it sent.

use std::collections::BTreeMap;
use std::fmt;

use anyhow::{Result, bail};
use fugazi::types::{Frequency, Timestamp};

use crate::calendar::{self, ScopedFrequency};
use crate::data::DataFrame;
use crate::style;

/// Bars a series needs before a declared-vs-detected mismatch is reported.
///
/// Detection medians the gaps, which is robust over a full series and noise
/// over a handful of bars: three daily equity bars spanning a weekend give
/// gaps of `1d` and `3d`, whose median is `3d`, which snaps to `1w`. Accusing a
/// correctly-labelled file on that evidence trains the user to ignore the
/// warning. Ten bars is enough for the weekday majority to carry the median.
const MIN_BARS_TO_ACCUSE: usize = 10;

/// How the cadence a series is *said* to run at was arrived at — the word the
/// mislabel warning uses to point at what to go fix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stated {
    /// The row's own `freq` column.
    Column,
    /// A `-f/--frequency` entry that matched this symbol.
    Flag,
}

impl fmt::Display for Stated {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Stated::Column => "the `freq` column",
            Stated::Flag => "`-f/--frequency`",
        })
    }
}

/// One `(symbol, freq cell)` group of the loaded frame, and the two answers to
/// "what cadence is this?" that can disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Series {
    pub symbol: String,
    /// The `freq` cell verbatim — `""` for a group whose rows carried no label.
    /// Kept as the raw string rather than the parsed cadence because it is also
    /// the frame's key, and because an unparseable label still groups.
    pub freq: String,
    /// `freq` parsed. `None` for an untagged group *or* an unparseable label —
    /// [`Series::is_labelled`] tells those apart.
    pub declared: Option<Frequency>,
    /// The cadence the timestamps imply. `None` when fewer than two rows carry
    /// a parseable `time`.
    pub detected: Option<Frequency>,
    /// Rows in the group that carry a parseable timestamp.
    pub bars: usize,
    /// Of those, how many land outside the calendar `time` can express (past
    /// year 9999, or before year -9999). See [`Finding::Undatable`].
    pub undatable: usize,
    /// One such stamp, verbatim, so a diagnostic can show its scale.
    pub undatable_example: Option<i64>,
}

impl Series {
    /// Build one group's entry from its symbol, `freq` cell and sorted stamps.
    pub fn new(symbol: impl Into<String>, freq: impl Into<String>, stamps: &[i64]) -> Self {
        let freq = freq.into();
        Self {
            symbol: symbol.into(),
            declared: (!freq.is_empty())
                .then(|| freq.parse::<Frequency>().ok())
                .flatten(),
            freq,
            detected: calendar::detect_frequency_from_millis(stamps.iter().copied()),
            bars: stamps.len(),
            undatable: stamps
                .iter()
                .filter(|&&ms| Timestamp(ms).to_datetime().is_none())
                .count(),
            undatable_example: stamps
                .iter()
                .copied()
                .find(|&ms| Timestamp(ms).to_datetime().is_none()),
        }
    }

    /// Whether the rows carried a `freq` cell at all — true even when the cell
    /// is something [`Frequency`] cannot parse, which is the case a bare
    /// `declared.is_some()` would misreport as untagged.
    pub fn is_labelled(&self) -> bool {
        !self.freq.is_empty()
    }

    /// The cadence to attribute to this group, best evidence first: the label,
    /// else the gaps. `-f` is *not* consulted here — it is applied by
    /// [`Census::resolve`], which is the only place that knows the flags.
    pub fn effective(&self) -> Option<Frequency> {
        self.declared.or(self.detected)
    }

    /// How to name this group in a diagnostic: its label, or `<untagged>` when
    /// it has none.
    pub fn label(&self) -> &str {
        if self.freq.is_empty() {
            "<untagged>"
        } else {
            &self.freq
        }
    }
}

/// Every cadence group in a loaded frame.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Census {
    /// Ascending by `(symbol, freq)`, matching the frame's own key order.
    pub series: Vec<Series>,
}

impl Census {
    /// Census a loaded frame — one pass over its keys.
    pub fn of(frame: &DataFrame) -> Self {
        Self::from_groups(
            frame
                .cadence_groups()
                .iter()
                .map(|(sym, freq, stamps)| Series::new(sym, freq, stamps)),
        )
    }

    /// The frame-free constructor, so the resolution rules can be tested
    /// against hand-built groups rather than temp CSVs.
    pub fn from_groups(groups: impl IntoIterator<Item = Series>) -> Self {
        let mut series: Vec<Series> = groups.into_iter().collect();
        series.sort_by(|a, b| (&a.symbol, &a.freq).cmp(&(&b.symbol, &b.freq)));
        Self { series }
    }

    /// Distinct symbols, ascending.
    pub fn symbols(&self) -> Vec<&str> {
        let mut out: Vec<&str> = self.series.iter().map(|s| s.symbol.as_str()).collect();
        out.dedup();
        out
    }

    /// Every group belonging to `symbol`.
    pub fn groups_of(&self, symbol: &str) -> Vec<&Series> {
        self.series.iter().filter(|s| s.symbol == symbol).collect()
    }

    /// Apply the `-f/--frequency` entries: decide which group of each symbol the
    /// run targets, and collect everything worth saying about the result.
    ///
    /// Pure — it neither reads nor prunes the frame. [`apply`] is the half that
    /// does, so the rules below can be exercised without one.
    pub fn resolve(&self, specs: &[ScopedFrequency]) -> Resolution {
        let mut keep: Vec<(String, String)> = Vec::new();
        let mut findings: Vec<Finding> = Vec::new();
        // Post-resolution cadence per symbol, for the mixed-universe check —
        // built from the *chosen* group so a disambiguated symbol contributes
        // the cadence it will actually run at, not both of them.
        let mut effective: Vec<(&str, Frequency)> = Vec::new();

        for symbol in self.symbols() {
            let groups = self.groups_of(symbol);
            let pick = calendar::pick_frequency(specs, symbol);
            let labelled: Vec<&Series> =
                groups.iter().copied().filter(|g| g.is_labelled()).collect();
            let untagged = groups.iter().copied().find(|g| !g.is_labelled());

            // Untagged rows beside labelled ones. Reachable only when the
            // symbol declares two or more cadences — with exactly one, the
            // loader folds untagged rows into it. There is no honest way to
            // attach them: dropping them loses a series the user supplied, and
            // guessing re-creates the bug this module exists to close.
            if let Some(untagged) = untagged
                && !labelled.is_empty()
            {
                findings.push(Finding::Untagged {
                    symbol: symbol.to_string(),
                    bars: untagged.bars,
                    declared: labelled.iter().map(|g| g.freq.clone()).collect(),
                });
                continue;
            }

            let chosen = match labelled.len() {
                // Sole group — untagged-only, or one labelled cadence.
                0 | 1 => {
                    let only = groups[0];
                    match (pick, only.declared) {
                        // The flag contradicts what the column says. Not a
                        // selection the frame can honour, and not a cadence we
                        // should quietly override a provider's own label with.
                        (Some(want), Some(declared)) if want != declared => {
                            findings.push(Finding::Absent {
                                symbol: symbol.to_string(),
                                requested: want,
                                available: vec![only.label().to_string()],
                            });
                            None
                        }
                        _ => Some(only),
                    }
                }
                // Two or more cadences under one name: the flag must choose.
                _ => match pick {
                    None => {
                        findings.push(Finding::Ambiguous {
                            symbol: symbol.to_string(),
                            cadences: labelled.iter().map(|g| (g.freq.clone(), g.bars)).collect(),
                        });
                        None
                    }
                    Some(want) => match labelled.iter().find(|g| g.declared == Some(want)) {
                        Some(g) => Some(*g),
                        None => {
                            findings.push(Finding::Absent {
                                symbol: symbol.to_string(),
                                requested: want,
                                available: labelled.iter().map(|g| g.freq.clone()).collect(),
                            });
                            None
                        }
                    },
                },
            };

            let Some(group) = chosen else { continue };
            keep.push((symbol.to_string(), group.freq.clone()));

            // What the series is *said* to run at — the column if it has one,
            // else the flag. Compared against the gaps.
            let stated = group
                .declared
                .map(|f| (f, Stated::Column))
                .or_else(|| pick.map(|f| (f, Stated::Flag)));
            if let (Some((stated, from)), Some(detected)) = (stated, group.detected)
                && stated != detected
                && group.bars >= MIN_BARS_TO_ACCUSE
            {
                findings.push(Finding::Mislabelled {
                    symbol: symbol.to_string(),
                    stated,
                    from,
                    detected,
                    bars: group.bars,
                });
            }

            if group.undatable > 0
                && let Some(example) = group.undatable_example
            {
                findings.push(Finding::Undatable {
                    symbol: symbol.to_string(),
                    freq: group.freq.clone(),
                    bars: group.undatable,
                    total: group.bars,
                    example,
                });
            }

            if let Some(f) = pick.or_else(|| group.effective()) {
                effective.push((symbol, f));
            }
        }

        // A universe that does not share one cadence. Reported once, listing
        // every cadence and who runs at it, rather than once per symbol.
        let mut by_cadence: BTreeMap<Frequency, Vec<String>> = BTreeMap::new();
        for (symbol, freq) in effective {
            by_cadence.entry(freq).or_default().push(symbol.to_string());
        }
        if by_cadence.len() > 1 {
            findings.push(Finding::Mixed {
                groups: by_cadence.into_iter().collect(),
            });
        }

        // Errors first: a caller that prints the lot wants the reason it is
        // about to stop above the advisory notes.
        findings.sort_by_key(|f| !f.is_error());
        Resolution { keep, findings }
    }
}

/// What [`Census::resolve`] decided, and what it wants to say about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    /// The `(symbol, freq cell)` group each resolvable symbol runs at. Symbols
    /// that produced an error are absent — there is nothing to keep.
    pub keep: Vec<(String, String)>,
    /// Errors first, then warnings.
    pub findings: Vec<Finding>,
}

impl Resolution {
    /// Whether anything here should stop the run.
    pub fn has_errors(&self) -> bool {
        self.findings.iter().any(Finding::is_error)
    }
}

/// One thing the census has to say about the input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Finding {
    /// A symbol carries two or more cadences and nothing chose between them.
    Ambiguous {
        symbol: String,
        /// `(freq cell, bars)`, ascending by cell.
        cadences: Vec<(String, usize)>,
    },
    /// `-f/--frequency` named a cadence this symbol does not carry.
    Absent {
        symbol: String,
        requested: Frequency,
        available: Vec<String>,
    },
    /// Rows with no `freq` label sit beside labelled ones under the same
    /// symbol, and the symbol has more than one label to attach them to.
    Untagged {
        symbol: String,
        bars: usize,
        declared: Vec<String>,
    },
    /// The universe runs at more than one cadence.
    Mixed {
        /// `(cadence, symbols)`, ascending by cadence duration.
        groups: Vec<(Frequency, Vec<String>)>,
    },
    /// A series has a `time` to read and **none of it parses** — so the column
    /// is not a time column at all.
    Untimed {
        symbol: String,
        freq: String,
        total: usize,
        /// One value, verbatim.
        example: String,
    },
    /// A series' `time` parses for some rows and not others.
    PartlyTimed {
        symbol: String,
        freq: String,
        unparsed: usize,
        total: usize,
        example: String,
    },
    /// A series' timestamps land outside the calendar, so nothing that reads a
    /// date can answer for them.
    Undatable {
        symbol: String,
        freq: String,
        bars: usize,
        total: usize,
        /// One offending stamp, verbatim, so the reader can see the scale.
        example: i64,
    },
    /// The frame is keyed by a numeric `index`, so it has no cadence to
    /// census. Informational, not a warning about the data.
    IndexSampled {
        /// How many bars the frame carries, across every symbol.
        bars: usize,
        /// Whether the rows still carry a parseable `time` column — which
        /// decides how much of the calendar layer keeps working.
        timed: bool,
    },
    /// A series' label and its timestamp spacing disagree.
    Mislabelled {
        symbol: String,
        stated: Frequency,
        from: Stated,
        detected: Frequency,
        bars: usize,
    },
}

impl Finding {
    /// Whether this stops the run. Ambiguity does; disagreement does not.
    pub fn is_error(&self) -> bool {
        matches!(
            self,
            Finding::Ambiguous { .. }
                | Finding::Absent { .. }
                | Finding::Untagged { .. }
                | Finding::Untimed { .. }
        )
    }
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Finding::IndexSampled { bars, timed } => {
                write!(
                    f,
                    "the input is index-sampled ({bars} bars keyed by a numeric `index`), so it \
                     has no bar cadence: annualization cannot be looked up from a calendar, \
                     `-w`'s duration form is unavailable, and freq-scoped `--costs` entries \
                     match nothing. "
                )?;
                if *timed {
                    f.write_str(
                        "The rows do carry a parseable `time` column, so carry is pro-rated over \
                         the interval each bar actually spans, the calendar leaves read real \
                         dates, and `bars_per_year` is measured from the span the input covers.",
                    )
                } else {
                    f.write_str(
                        "The rows carry no parseable `time`, so carry charges nothing, the \
                         calendar leaves read `None` on every bar, and `--bars-per-year` has to \
                         be passed explicitly.",
                    )
                }
            }
            Finding::Ambiguous { symbol, cadences } => {
                let list = cadences
                    .iter()
                    .map(|(freq, bars)| format!("{freq} ({bars} bars)"))
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(
                    f,
                    "symbol `{symbol}` carries {} cadences in the input series — {list}. \
                     A strategy trades one of them; pass `-f/--frequency {symbol}:<CODE>` to \
                     say which, or narrow the input.",
                    cadences.len(),
                )
            }
            Finding::Absent {
                symbol,
                requested,
                available,
            } => write!(
                f,
                "`-f/--frequency` asks for `{requested}` on `{symbol}`, but the input series \
                 carry only {} for it: {}. Nothing would be traded at `{requested}`.",
                if available.len() == 1 {
                    "one cadence"
                } else {
                    "these cadences"
                },
                available.join(", "),
            ),
            Finding::Untagged {
                symbol,
                bars,
                declared,
            } => write!(
                f,
                "symbol `{symbol}` has {bars} row(s) with no `freq` label beside {} labelled \
                 cadences ({}), so those rows cannot be attached to either. Give them one with \
                 a series literal — `--series \"freq=<CODE>,@file.csv\"`.",
                declared.len(),
                declared.join(", "),
            ),
            Finding::Untimed {
                symbol,
                freq,
                total,
                example,
            } => write!(
                f,
                "`{symbol}`{}: none of its {total} `time` value(s) parse as a timestamp \
                 (e.g. `{example}`).\n\
                 \n\
                 A column named `time` promises timestamps, and everything time-denominated \
                 reads it: carry is pro-rated over it, the calendar leaves (`!day_of_week`, \
                 `!is_weekday`, …) decompose it, and `bars_per_year` is measured from the \
                 span it covers. Unparsed, all three go quiet without failing — the run \
                 completes and reports a strategy that was never charged for carry.\n\
                 \n\
                 fugazi reads RFC 3339 (`2024-01-01T00:00:00Z`), `YYYY-MM-DD [HH:MM:SS]`, or \
                 an integer Unix epoch in seconds or milliseconds. If these are not times at \
                 all — a bar sequence, a session id — name the column `index` instead, which \
                 orders and joins on them without claiming they are dates.",
                if freq.is_empty() {
                    String::new()
                } else {
                    format!(" [{freq}]")
                },
            ),
            Finding::PartlyTimed {
                symbol,
                freq,
                unparsed,
                total,
                example,
            } => write!(
                f,
                "`{symbol}`{}: {unparsed} of {total} `time` value(s) do not parse as a \
                 timestamp (e.g. `{example}`), so those bars carry no time. Carry is not \
                 charged across them, the calendar leaves read `None` there, and the cadence \
                 detected from the gaps is measured over the rows that did parse — which is \
                 a different series from the one in the file.",
                if freq.is_empty() {
                    String::new()
                } else {
                    format!(" [{freq}]")
                },
            ),
            Finding::Undatable {
                symbol,
                freq,
                bars,
                total,
                example,
            } => write!(
                f,
                "`{symbol}`{}: {bars} of {total} timestamp(s) fall outside the calendar \
                 (`{example}`), so every calendar reading — `!is_weekday`, `!month`, \
                 `!day_of_week` — is absent for them and any gate built on one never fires. \
                 A `time` column in **nanoseconds** is the usual cause: `datetime64[ns]` cast \
                 to an integer. fugazi reads milliseconds; divide by 1e6.",
                if freq.is_empty() {
                    String::new()
                } else {
                    format!(" [{freq}]")
                },
            ),
            Finding::Mixed { groups } => {
                let list = groups
                    .iter()
                    .map(|(freq, syms)| {
                        format!("{freq}: {}", crate::overlap::format_symbol_list(syms))
                    })
                    .collect::<Vec<_>>()
                    .join(" · ");
                write!(
                    f,
                    "the input universe runs at {} different cadences — {list}. \
                     Annualization (`bars_per_year`), `-w`'s duration form and \
                     `trading_seconds_per_bar` all take one cadence for the whole run, read off \
                     the first symbol, so every risk-adjusted metric here is scaled for that \
                     series and mis-scaled for the rest.",
                    groups.len(),
                )
            }
            Finding::Mislabelled {
                symbol,
                stated,
                from,
                detected,
                bars,
            } => write!(
                f,
                "`{symbol}` is labelled `{stated}` by {from}, but its {bars} timestamps are \
                 spaced like `{detected}`. The label is what freq-scoped `--costs` match on; \
                 the spacing is what the run is really made of.",
            ),
        }
    }
}

/// Census `frame`, resolve `-f/--frequency` against it, prune the frame to the
/// chosen cadence per symbol, and hand back the warnings.
///
/// Errors abort with every error finding in one message: a frame with three
/// ambiguous symbols should not take three runs to fix.
/// Census the `time` column: a name that declares a type is checked against it.
///
/// Split on the module's own rule. **None parsing is ambiguity** — the column
/// is not a time column, and there is no reading of it that is right, so it is
/// refused and the error names `index` as the home for keys that are not dates.
/// **Some parsing is disagreement** — the run is well-defined, just quietly
/// missing time on those bars, so it is warned.
///
/// Runs before the cadence census proper and before the index-sampled
/// short-circuit: an index-keyed frame may still carry a `time` column, and it
/// is exactly the frame where a silently-unread one costs the most, since
/// measured annualization is the only annualization it has.
fn census_times(frame: &DataFrame) -> Vec<Finding> {
    let mut out = Vec::new();
    for group in frame.time_census() {
        let Some(example) = group.example.clone() else {
            continue;
        };
        let unparsed = group.with_cell - group.parsed;
        if group.parsed == 0 {
            out.push(Finding::Untimed {
                symbol: group.symbol.to_string(),
                freq: group.freq.to_string(),
                total: group.with_cell,
                example,
            });
        } else {
            out.push(Finding::PartlyTimed {
                symbol: group.symbol.to_string(),
                freq: group.freq.to_string(),
                unparsed,
                total: group.with_cell,
                example,
            });
        }
    }
    out
}

pub fn apply(frame: &mut DataFrame, specs: &[ScopedFrequency]) -> Result<Vec<Finding>> {
    let time_findings = census_times(frame);
    if time_findings.iter().any(Finding::is_error) {
        let errors: Vec<String> = time_findings
            .iter()
            .filter(|f| f.is_error())
            .map(Finding::to_string)
            .collect();
        bail!("{}", errors.join("\n\n"));
    }
    // An index-sampled frame has no cadence to census, and every finding below
    // is *about* a cadence. Detection would still produce one — the median gap
    // between dollar-bar closes snaps to some named `Frequency` like any other
    // number — and it would be an artefact of how busy the tape was, reported
    // with the same confidence as a real one. Say what the input is instead.
    if frame.is_index_sampled() {
        let mut out = time_findings;
        out.push(Finding::IndexSampled {
            bars: frame.len(),
            timed: frame.has_parseable_times(),
        });
        return Ok(out);
    }
    let resolution = Census::of(frame).resolve(specs);
    if resolution.has_errors() {
        let errors: Vec<String> = resolution
            .findings
            .iter()
            .filter(|f| f.is_error())
            .map(Finding::to_string)
            .collect();
        bail!("{}", errors.join("\n\n"));
    }
    let mut findings = time_findings;
    findings.extend(resolution.findings);
    for (symbol, freq) in &resolution.keep {
        // Only touch symbols that actually had a choice — `retain_cadence`
        // rebuilds the memoized schema, and a single-cadence frame (the
        // overwhelming majority) should not pay for it.
        if frame.frequencies_of(symbol).len() > 1 {
            frame.retain_cadence(symbol, freq);
        }
    }
    Ok(findings)
}

/// Print the warnings to stderr, one paragraph each.
///
/// Not gated on `--quiet`, and on stderr rather than in the inputs block, for
/// the same reason [`crate::overlap::warn_if_fragmented`] is not: these say the
/// run is about to measure something other than what it looks like it is
/// measuring. Errors are unreachable here — [`apply`] has already bailed on
/// them — but they print the same way if a caller resolves by hand.
pub fn warn(findings: &[Finding]) {
    for finding in findings {
        eprintln!("  {} {finding}", style::yellow("warn"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: i64 = 3_600_000;
    const DAY: i64 = 86_400_000;

    /// `n` stamps spaced `step` apart, starting at an arbitrary fixed epoch.
    fn spaced(step: i64, n: usize) -> Vec<i64> {
        (0..n as i64)
            .map(|i| 1_704_067_200_000 + i * step)
            .collect()
    }

    /// A group whose cadence is unambiguous and matches its label — the shape
    /// most of these tests want everywhere except the one place under test.
    fn honest(symbol: &str, freq: &str, step: i64) -> Series {
        Series::new(symbol, freq, &spaced(step, 40))
    }

    /// `-f/--frequency` entries from their CLI spellings.
    fn flags(specs: &[&str]) -> Vec<ScopedFrequency> {
        specs
            .iter()
            .map(|s| s.parse().expect("test flag parses"))
            .collect()
    }

    fn kept(res: &Resolution) -> Vec<(&str, &str)> {
        res.keep
            .iter()
            .map(|(s, f)| (s.as_str(), f.as_str()))
            .collect()
    }

    // ---------------------------------------------------------------- Series

    #[test]
    fn a_group_reads_its_label_and_its_gaps_independently() {
        let s = Series::new("BTC", "1d", &spaced(DAY, 30));
        assert_eq!(s.declared, Some(Frequency::Day(1)));
        assert_eq!(s.detected, Some(Frequency::Day(1)));
        assert_eq!(s.effective(), Some(Frequency::Day(1)));
        assert_eq!(s.bars, 30);
        assert!(s.is_labelled());
        assert_eq!(s.label(), "1d");
    }

    /// The label wins over the gaps — a provider that told us the cadence
    /// outranks arithmetic on the bars it sent.
    #[test]
    fn a_label_outranks_the_detected_spacing() {
        let s = Series::new("BTC", "1d", &spaced(HOUR, 30));
        assert_eq!(s.effective(), Some(Frequency::Day(1)));
        assert_eq!(s.detected, Some(Frequency::Hour(1)));
    }

    #[test]
    fn an_untagged_group_falls_back_to_its_gaps() {
        let s = Series::new("BTC", "", &spaced(HOUR, 30));
        assert!(!s.is_labelled());
        assert_eq!(s.declared, None);
        assert_eq!(s.effective(), Some(Frequency::Hour(1)));
        assert_eq!(s.label(), "<untagged>");
    }

    /// A cell `Frequency` cannot parse is still a *label* — the group is
    /// tagged, we just don't know as what. Reading `declared.is_none()` as
    /// "untagged" would silently fold it in with the unlabelled rows.
    #[test]
    fn an_unparseable_label_is_labelled_but_undeclared() {
        let s = Series::new("BTC", "weekly", &spaced(DAY * 7, 30));
        assert!(s.is_labelled());
        assert_eq!(s.declared, None);
        assert_eq!(s.effective(), Some(Frequency::Week(1)));
        assert_eq!(s.label(), "weekly");
    }

    #[test]
    fn one_bar_detects_nothing() {
        let s = Series::new("BTC", "", &spaced(DAY, 1));
        assert_eq!(s.detected, None);
        assert_eq!(s.effective(), None);
        assert_eq!(s.bars, 1);
    }

    /// `1M` is a month and `1m` is a minute. Case-folding the cell anywhere in
    /// this pipeline would fuse two cadences four orders of magnitude apart.
    #[test]
    fn month_and_minute_are_not_the_same_label() {
        assert_eq!(
            Series::new("X", "1M", &[]).declared,
            Some(Frequency::Month(1))
        );
        assert_eq!(
            Series::new("X", "1m", &[]).declared,
            Some(Frequency::Minute(1))
        );
        let census = Census::from_groups([
            Series::new("X", "1M", &spaced(DAY * 30, 20)),
            Series::new("X", "1m", &spaced(60_000, 20)),
        ]);
        assert_eq!(census.groups_of("X").len(), 2);
    }

    // ------------------------------------------------------- the quiet cases

    #[test]
    fn a_single_labelled_series_resolves_silently() {
        let res = Census::from_groups([honest("BTC", "1d", DAY)]).resolve(&[]);
        assert_eq!(kept(&res), [("BTC", "1d")]);
        assert_eq!(res.findings, []);
    }

    #[test]
    fn a_single_untagged_series_resolves_silently() {
        let res = Census::from_groups([honest("BTC", "", DAY)]).resolve(&[]);
        assert_eq!(kept(&res), [("BTC", "")]);
        assert_eq!(res.findings, []);
    }

    /// Every symbol at one cadence is the ordinary case, however many there
    /// are — nothing to say.
    #[test]
    fn a_uniform_universe_says_nothing() {
        let res = Census::from_groups([
            honest("BTC", "1d", DAY),
            honest("ETH", "1d", DAY),
            honest("SOL", "1d", DAY),
        ])
        .resolve(&[]);
        assert_eq!(kept(&res), [("BTC", "1d"), ("ETH", "1d"), ("SOL", "1d")]);
        assert_eq!(res.findings, []);
    }

    /// A flag that agrees with the label is a no-op, not a contradiction.
    #[test]
    fn a_flag_that_agrees_with_the_label_is_silent() {
        let res = Census::from_groups([honest("BTC", "1d", DAY)]).resolve(&flags(&["BTC:1d"]));
        assert_eq!(kept(&res), [("BTC", "1d")]);
        assert_eq!(res.findings, []);
    }

    /// The historical use of `-f`: an input with no `freq` column, where the
    /// flag *declares* the cadence rather than selecting one. Nothing to
    /// contradict, so nothing is said.
    #[test]
    fn a_flag_over_an_untagged_series_declares_rather_than_contradicts() {
        let res = Census::from_groups([honest("BTC", "", DAY)]).resolve(&flags(&["1d"]));
        assert_eq!(kept(&res), [("BTC", "")]);
        assert_eq!(res.findings, []);
    }

    // -------------------------------------------------------------- ambiguity

    #[test]
    fn two_cadences_under_one_symbol_is_an_error() {
        let res = Census::from_groups([
            Series::new("BTC", "1d", &spaced(DAY, 12)),
            Series::new("BTC", "1h", &spaced(HOUR, 48)),
        ])
        .resolve(&[]);
        assert!(res.has_errors());
        assert_eq!(kept(&res), []);
        assert_eq!(
            res.findings,
            [Finding::Ambiguous {
                symbol: "BTC".into(),
                cadences: vec![("1d".into(), 12), ("1h".into(), 48)],
            }]
        );
        // The message names both cadences with their weight, so the user can
        // tell the series they wanted from the one that leaked in.
        let text = res.findings[0].to_string();
        assert!(text.contains("1d (12 bars), 1h (48 bars)"), "{text}");
        assert!(text.contains("-f/--frequency BTC:<CODE>"), "{text}");
    }

    #[test]
    fn a_symbol_scoped_flag_picks_one_of_two_cadences() {
        let census = Census::from_groups([
            Series::new("BTC", "1d", &spaced(DAY, 12)),
            Series::new("BTC", "1h", &spaced(HOUR, 48)),
        ]);
        let res = census.resolve(&flags(&["BTC:1h"]));
        assert!(!res.has_errors());
        assert_eq!(kept(&res), [("BTC", "1h")]);
        assert_eq!(res.findings, []);
        // …and the other way, so the pick is really being read.
        assert_eq!(kept(&census.resolve(&flags(&["BTC:1d"]))), [("BTC", "1d")]);
    }

    /// Ambiguity is per symbol: one symbol's mess doesn't stop the others
    /// resolving, and the run still refuses because of the one.
    #[test]
    fn ambiguity_is_reported_per_symbol_and_the_rest_still_resolve() {
        let res = Census::from_groups([
            Series::new("BTC", "1d", &spaced(DAY, 12)),
            Series::new("BTC", "1h", &spaced(HOUR, 48)),
            honest("ETH", "1d", DAY),
        ])
        .resolve(&[]);
        assert!(res.has_errors());
        assert_eq!(kept(&res), [("ETH", "1d")]);
        assert_eq!(res.findings.len(), 1);
    }

    /// Two ambiguous symbols produce two findings, so one pass over the input
    /// tells the user everything they have to fix.
    #[test]
    fn every_ambiguous_symbol_is_reported_in_one_pass() {
        let res = Census::from_groups([
            Series::new("BTC", "1d", &spaced(DAY, 12)),
            Series::new("BTC", "1h", &spaced(HOUR, 48)),
            Series::new("ETH", "1d", &spaced(DAY, 12)),
            Series::new("ETH", "4h", &spaced(HOUR * 4, 48)),
        ])
        .resolve(&[]);
        assert_eq!(res.findings.len(), 2);
        assert!(res.findings.iter().all(Finding::is_error));
    }

    /// An unparseable label can be grouped but not *selected* — no flag value
    /// can equal it, so it stays reportable rather than becoming un-fixable
    /// silence.
    #[test]
    fn an_unparseable_label_cannot_be_selected_by_a_flag() {
        let res = Census::from_groups([
            Series::new("BTC", "weekly", &spaced(DAY * 7, 30)),
            Series::new("BTC", "1d", &spaced(DAY, 30)),
        ])
        .resolve(&flags(&["BTC:1w"]));
        assert!(res.has_errors());
        assert!(matches!(res.findings[0], Finding::Absent { .. }));
    }

    // ----------------------------------------------------------- absent picks

    #[test]
    fn a_flag_naming_a_cadence_the_frame_lacks_is_an_error() {
        let res = Census::from_groups([
            Series::new("BTC", "1d", &spaced(DAY, 12)),
            Series::new("BTC", "1h", &spaced(HOUR, 48)),
        ])
        .resolve(&flags(&["BTC:5m"]));
        assert_eq!(
            res.findings,
            [Finding::Absent {
                symbol: "BTC".into(),
                requested: Frequency::Minute(5),
                available: vec!["1d".into(), "1h".into()],
            }]
        );
        assert_eq!(kept(&res), []);
        let text = res.findings[0].to_string();
        assert!(text.contains("these cadences for it: 1d, 1h"), "{text}");
    }

    /// The single-group form of the same mistake: `-f BTC:1h` on a frame whose
    /// BTC rows all say `1d`. Silently annualizing at 1h was the old
    /// behaviour; contradicting the provider's own label is not something to
    /// do quietly.
    #[test]
    fn a_flag_contradicting_the_only_declared_cadence_is_an_error() {
        let res = Census::from_groups([honest("BTC", "1d", DAY)]).resolve(&flags(&["BTC:1h"]));
        assert_eq!(
            res.findings,
            [Finding::Absent {
                symbol: "BTC".into(),
                requested: Frequency::Hour(1),
                available: vec!["1d".into()],
            }]
        );
        let text = res.findings[0].to_string();
        assert!(text.contains("only one cadence for it: 1d"), "{text}");
    }

    /// An *unscoped* `-f` is a default for the whole run, and contradicting a
    /// labelled symbol with it is the same mistake as the scoped form — the
    /// flag is still what the user typed.
    #[test]
    fn an_unscoped_flag_contradicting_a_label_is_also_an_error() {
        let res = Census::from_groups([honest("BTC", "1d", DAY)]).resolve(&flags(&["4h"]));
        assert!(res.has_errors());
    }

    #[test]
    fn a_symbol_scoped_flag_beats_the_unscoped_default() {
        let res = Census::from_groups([
            Series::new("BTC", "1d", &spaced(DAY, 12)),
            Series::new("BTC", "1h", &spaced(HOUR, 48)),
        ])
        .resolve(&flags(&["1d", "BTC:1h"]));
        assert_eq!(kept(&res), [("BTC", "1h")]);
        assert_eq!(res.findings, []);
    }

    // --------------------------------------------------------- untagged rows

    /// Untagged rows can only sit *beside* labelled ones when the symbol has
    /// two or more labels — with exactly one the loader folds them in. There
    /// is nothing to attach them to, so this refuses rather than dropping a
    /// series the user supplied.
    #[test]
    fn untagged_rows_beside_two_labels_are_an_error() {
        let res = Census::from_groups([
            Series::new("BTC", "", &spaced(DAY, 12)),
            Series::new("BTC", "1d", &spaced(DAY, 12)),
            Series::new("BTC", "1h", &spaced(HOUR, 48)),
        ])
        .resolve(&flags(&["BTC:1d"]));
        assert_eq!(
            res.findings,
            [Finding::Untagged {
                symbol: "BTC".into(),
                bars: 12,
                declared: vec!["1d".into(), "1h".into()],
            }]
        );
        // Nothing is kept: honouring the flag here would drop the 12 untagged
        // rows without saying so, which is the failure mode this module exists
        // to close.
        assert_eq!(kept(&res), []);
        let text = res.findings[0].to_string();
        assert!(text.contains("freq=<CODE>"), "{text}");
    }

    // --------------------------------------------------------- mislabelling

    #[test]
    fn a_label_that_disagrees_with_the_spacing_is_reported() {
        let res = Census::from_groups([Series::new("BTC", "1d", &spaced(HOUR, 40))]).resolve(&[]);
        assert_eq!(
            res.findings,
            [Finding::Mislabelled {
                symbol: "BTC".into(),
                stated: Frequency::Day(1),
                from: Stated::Column,
                detected: Frequency::Hour(1),
                bars: 40,
            }]
        );
        // A warning, not an error — the run is measurable, just mis-scaled.
        assert!(!res.has_errors());
        assert_eq!(kept(&res), [("BTC", "1d")]);
        let text = res.findings[0].to_string();
        assert!(
            text.contains("labelled `1d` by the `freq` column"),
            "{text}"
        );
        assert!(text.contains("spaced like `1h`"), "{text}");
    }

    /// With no label, `-f` is what the series is *said* to run at, so it gets
    /// checked against the gaps the same way — and the message points at the
    /// flag rather than at a column that isn't there.
    #[test]
    fn a_flag_that_disagrees_with_the_spacing_is_reported_against_the_flag() {
        let res = Census::from_groups([Series::new("BTC", "", &spaced(HOUR, 40))])
            .resolve(&flags(&["1d"]));
        assert_eq!(
            res.findings,
            [Finding::Mislabelled {
                symbol: "BTC".into(),
                stated: Frequency::Day(1),
                from: Stated::Flag,
                detected: Frequency::Hour(1),
                bars: 40,
            }]
        );
        assert!(
            res.findings[0].to_string().contains("`-f/--frequency`"),
            "{}",
            res.findings[0]
        );
    }

    /// Detection medians the gaps, which is noise over a handful of bars:
    /// three daily equity bars across a weekend median to `3d` and snap to
    /// `1w`. Accusing a correctly-labelled file on that evidence would train
    /// the user to ignore the warning.
    #[test]
    fn a_short_series_is_not_accused_of_mislabelling() {
        let short = Series::new("BTC", "1d", &spaced(HOUR, MIN_BARS_TO_ACCUSE - 1));
        assert_eq!(short.detected, Some(Frequency::Hour(1)));
        assert_eq!(Census::from_groups([short]).resolve(&[]).findings, []);

        // One more bar and it is enough evidence to say so.
        let long = Series::new("BTC", "1d", &spaced(HOUR, MIN_BARS_TO_ACCUSE));
        assert_eq!(Census::from_groups([long]).resolve(&[]).findings.len(), 1);
    }

    /// Weekends are a minority of the gaps, so a real daily equity series
    /// medians to `1d` and is not accused.
    #[test]
    fn weekend_gaps_do_not_make_a_daily_series_look_mislabelled() {
        // Four weeks of Mon–Fri: four 1d gaps then a 3d gap, repeating.
        let mut stamps = vec![1_704_067_200_000];
        for week in 0..4 {
            for day in 0..5 {
                let last = *stamps.last().expect("seeded");
                let step = if week > 0 && day == 0 { DAY * 3 } else { DAY };
                stamps.push(last + step);
            }
        }
        let s = Series::new("SPY", "1d", &stamps);
        assert_eq!(s.detected, Some(Frequency::Day(1)));
        assert_eq!(Census::from_groups([s]).resolve(&[]).findings, []);
    }

    /// A label with no timestamps to check it against is taken at its word.
    #[test]
    fn a_label_with_no_parseable_stamps_is_not_accused() {
        let res = Census::from_groups([Series::new("BTC", "1d", &[])]).resolve(&[]);
        assert_eq!(res.findings, []);
        assert_eq!(kept(&res), [("BTC", "1d")]);
    }

    // ------------------------------------------------------ mixed universes

    #[test]
    fn a_universe_at_two_cadences_is_reported_once() {
        let res = Census::from_groups([
            honest("BTC", "1d", DAY),
            honest("ETH", "1h", HOUR),
            honest("SOL", "1h", HOUR),
        ])
        .resolve(&[]);
        assert_eq!(
            res.findings,
            [Finding::Mixed {
                groups: vec![
                    (Frequency::Hour(1), vec!["ETH".into(), "SOL".into()]),
                    (Frequency::Day(1), vec!["BTC".into()]),
                ],
            }]
        );
        assert!(!res.has_errors());
        let text = res.findings[0].to_string();
        assert!(text.contains("1h: ETH, SOL"), "{text}");
        assert!(text.contains("1d: BTC"), "{text}");
    }

    /// The mixed check runs on the cadence each symbol *resolved to*, so a
    /// disambiguated symbol contributes the one it will trade rather than both
    /// of the ones it carried.
    #[test]
    fn the_mixed_check_reads_the_chosen_cadence_not_every_candidate() {
        let census = Census::from_groups([
            Series::new("BTC", "1d", &spaced(DAY, 30)),
            Series::new("BTC", "1h", &spaced(HOUR, 30)),
            honest("ETH", "1h", HOUR),
        ]);
        // Choosing BTC's 1h leaves a uniform universe: no mixed warning.
        let unified = census.resolve(&flags(&["BTC:1h"]));
        assert_eq!(unified.findings, []);
        // Choosing BTC's 1d makes it genuinely mixed.
        let split = census.resolve(&flags(&["BTC:1d"]));
        assert_eq!(split.findings.len(), 1);
        assert!(matches!(split.findings[0], Finding::Mixed { .. }));
    }

    /// A symbol whose cadence is unknowable contributes nothing to the mixed
    /// check rather than a third pseudo-cadence.
    #[test]
    fn a_symbol_with_no_knowable_cadence_is_left_out_of_the_mixed_check() {
        let res = Census::from_groups([
            honest("BTC", "1d", DAY),
            honest("ETH", "1d", DAY),
            Series::new("XRP", "", &spaced(DAY, 1)),
        ])
        .resolve(&[]);
        assert_eq!(res.findings, []);
    }

    /// A universe that is mixed *and* has an ambiguous symbol reports the
    /// error above the warning, because the error is why it is about to stop.
    #[test]
    fn errors_sort_above_warnings() {
        let res = Census::from_groups([
            Series::new("BTC", "1d", &spaced(DAY, 30)),
            Series::new("BTC", "4h", &spaced(HOUR * 4, 30)),
            honest("ETH", "1h", HOUR),
            honest("SOL", "1d", DAY),
        ])
        .resolve(&[]);
        assert!(res.findings.len() >= 2);
        assert!(res.findings[0].is_error());
        assert!(!res.findings.last().expect("non-empty").is_error());
    }

    // ------------------------------------------------------------ the census

    #[test]
    fn the_census_orders_groups_by_symbol_then_label() {
        let census = Census::from_groups([
            honest("ETH", "1h", HOUR),
            honest("BTC", "1h", HOUR),
            honest("BTC", "1d", DAY),
        ]);
        let seen: Vec<(&str, &str)> = census
            .series
            .iter()
            .map(|s| (s.symbol.as_str(), s.freq.as_str()))
            .collect();
        assert_eq!(seen, [("BTC", "1d"), ("BTC", "1h"), ("ETH", "1h")]);
        assert_eq!(census.symbols(), ["BTC", "ETH"]);
        assert_eq!(census.groups_of("BTC").len(), 2);
        assert_eq!(census.groups_of("DOGE").len(), 0);
    }

    #[test]
    fn an_empty_frame_censuses_to_nothing() {
        let res = Census::default().resolve(&flags(&["1d"]));
        assert_eq!(res.keep, []);
        assert_eq!(res.findings, []);
        assert!(!res.has_errors());
    }
}
