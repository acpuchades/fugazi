//! `--from` / `--until`: which bars a run **evaluates**.
//!
//! The interval is half-open — `[from, until)` — so adjacent ranges tile
//! exactly. `--until 2025-02-01` on a development run and `--from 2025-02-01`
//! on the run that scores it partition the series with no bar counted twice
//! and none dropped between them.
//!
//! # `--from` bounds evaluation, not loading
//!
//! The distinction is invisible in the output and both readings are
//! defensible, so it is worth stating plainly: bars *before* `--from` are still
//! read when the series carries them. They are fed to the strategy with the
//! trade step gated off — every indicator advances, no order is submitted, no
//! equity is booked — so the first evaluated bar is measured on settled
//! indicators rather than on a cold chain. Without that, the first
//! `max(stable_bars)` bars of a sliced run would be missing or silently wrong,
//! and a sliced run would not be comparable to an unsliced one, which is the
//! whole point of slicing.
//!
//! How far back it reads is `max(stable_bars)` — the same quantity
//! `--walkforward` skips at the head of a series, asked of the same built
//! strategy, so the two agree on what "settled" means.
//!
//! Two escape hatches, in the crate's usual shape of a safe default plus one
//! named opt-out:
//!
//! - **Not enough prior data** (the series starts at or near `--from`): the run
//!   warns and *starts late*, at the first bar that is settled. The effective
//!   start is what lands in `metrics.yml`'s `period_start`, so the artifact
//!   records what happened rather than what was asked for.
//! - **`--strict-from`**: a hard slice. Nothing before `--from` is read at all
//!   and the strategy starts cold — for deliberately simulating a cold start,
//!   which is the one case where the warm-up read-back is the wrong answer.

use anyhow::{Result, bail};

use crate::calendar;

/// A parsed `--from` / `--until` pair, as epoch milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DateRange {
    /// Inclusive lower bound on the evaluated range.
    from: Option<i64>,
    /// Exclusive upper bound.
    until: Option<i64>,
    /// Read nothing before `from`; start the strategy cold.
    strict: bool,
}

/// Where a [`DateRange`] lands on a concrete bar-label stream.
///
/// The three indices carve the stream into `[..warm_start)` dropped,
/// `[warm_start..eval_start)` warmed but not measured, and
/// `[eval_start..end)` evaluated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Slice {
    /// First bar handed to the strategy at all.
    pub warm_start: usize,
    /// First bar that is *measured*. Equal to `warm_start` when nothing is
    /// being warmed, and greater than [`Self::requested`] when the series did
    /// not reach far enough back to settle by then.
    pub eval_start: usize,
    /// One past the last bar handed to the strategy.
    pub end: usize,
    /// Where `--from` asked evaluation to begin. Also, and not by coincidence,
    /// the number of bars the series carries *before* that boundary — which is
    /// exactly the history available to warm with.
    pub requested: usize,
}

impl Slice {
    /// The whole stream, evaluated end to end — what an unsliced run does.
    pub(crate) fn everything(n: usize) -> Self {
        Slice {
            warm_start: 0,
            eval_start: 0,
            end: n,
            requested: 0,
        }
    }

    /// Bars fed to the strategy but not measured.
    pub(crate) fn warmup_bars(&self) -> usize {
        self.eval_start - self.warm_start
    }

    /// Whether this slice keeps the stream whole — the cheap test that lets a
    /// caller skip the copy entirely.
    pub(crate) fn is_everything(&self, n: usize) -> bool {
        self.warm_start == 0 && self.eval_start == 0 && self.end == n
    }

    /// The half-open span handed to the strategy, warm-up prefix included.
    pub(crate) fn fed(&self) -> std::ops::Range<usize> {
        self.warm_start..self.end
    }
}

impl DateRange {
    /// Parse the three flags. `None` when neither bound was given — the caller
    /// then skips every code path here, so an unsliced run behaves exactly as
    /// it did before these flags existed.
    ///
    /// `--strict-from` without `--from` is refused rather than ignored: it
    /// would silently do nothing, and a flag that reads as a safety measure
    /// must not be a no-op.
    pub(crate) fn parse(
        from: Option<&str>,
        until: Option<&str>,
        strict: bool,
    ) -> Result<Option<Self>> {
        if from.is_none() && until.is_none() {
            if strict {
                bail!(
                    "`--strict-from` needs `--from` — there is no boundary for it to be strict about"
                );
            }
            return Ok(None);
        }
        let parse_one = |raw: Option<&str>, flag: &str| -> Result<Option<i64>> {
            raw.map(|s| {
                calendar::parse_time_to_millis(s).ok_or_else(|| {
                    anyhow::anyhow!(
                        "`{flag} {s}` is not a date — expected `YYYY-MM-DD`, \
                         `YYYY-MM-DD HH:MM:SS`, or an RFC 3339 timestamp"
                    )
                })
            })
            .transpose()
        };
        let from = parse_one(from, "--from")?;
        let until = parse_one(until, "--until")?;
        if let (Some(f), Some(u)) = (from, until)
            && u <= f
        {
            bail!(
                "`--until` must be strictly after `--from` — the range is \
                 half-open `[from, until)`, so an equal pair selects no bars"
            );
        }
        Ok(Some(DateRange {
            from,
            until,
            strict,
        }))
    }

    /// Whether a warm-up read-back applies at all.
    pub(crate) fn reads_back(&self) -> bool {
        self.from.is_some() && !self.strict
    }

    /// Resolve against a bar-label stream and a warm-up requirement.
    ///
    /// `bars` must be ascending by time — every producer here walks a
    /// `BTreeMap`, so it is by construction. `warmup_need` is the built
    /// strategy's `stable_bars()`; pass `0` to skip the read-back.
    ///
    /// Errors when the range selects no bar at all: a run over an empty series
    /// produces a `metrics.yml` full of degenerate zeros, which reads like a
    /// strategy that did nothing rather than like a mistyped date.
    pub(crate) fn resolve(&self, bars: &[String], warmup_need: usize) -> Result<Slice> {
        let stamp = |s: &String| calendar::parse_time_to_millis(s);
        // A label the calendar cannot parse sorts as "before everything", which
        // would silently pull unparseable bars into the range. Treating it as
        // out of range in both directions is the conservative reading.
        let at_or_after =
            |t: i64| -> usize { bars.partition_point(|b| stamp(b).is_some_and(|ms| ms < t)) };

        let requested = self.from.map_or(0, at_or_after);
        let end = self.until.map_or(bars.len(), at_or_after);
        if requested >= end {
            bail!(
                "`--from`/`--until` select no bars of the {} the input carries \
                 ({}) — check the range against the series",
                plural(bars.len(), "bar"),
                span_of(bars),
            );
        }

        // A hard slice: nothing before the boundary is read, so evaluation
        // starts exactly where it was asked to and the chains start cold.
        if self.strict || self.from.is_none() {
            return Ok(Slice {
                warm_start: requested,
                eval_start: requested,
                end,
                requested,
            });
        }

        // Settle first, evaluate second. When the series does not reach far
        // enough back, evaluation starts late rather than starting unsettled —
        // the caller warns, and `period_start` records where it actually began.
        let eval_start = requested.max(warmup_need).min(end);
        Ok(Slice {
            warm_start: eval_start.saturating_sub(warmup_need),
            eval_start,
            end,
            requested,
        })
    }
}

/// `n unit` / `n units`.
fn plural(n: usize, unit: &str) -> String {
    if n == 1 {
        format!("{n} {unit}")
    } else {
        format!("{n} {unit}s")
    }
}

/// `first → last` of a label stream, for an error that has to say what *was*
/// available.
fn span_of(bars: &[String]) -> String {
    match (bars.first(), bars.last()) {
        (Some(f), Some(l)) => format!("{f} → {l}"),
        _ => "empty".to_string(),
    }
}

/// The warning for a run whose evaluation slipped past `--from` because the
/// series did not reach far enough back to settle by then.
///
/// Returns `None` when evaluation began exactly where it was asked to — which
/// includes the ordinary case of a full read-back, where the warm-up is short
/// of nothing.
pub(crate) fn short_warmup_warning(
    slice: &Slice,
    bars: &[String],
    requested_label: &str,
    warmup_need: usize,
) -> Option<String> {
    if slice.eval_start <= slice.requested {
        return None;
    }
    let started = bars.get(slice.eval_start)?;
    Some(format!(
        "only {} precede `--from {requested_label}`, but {} are needed to settle \
         this strategy; evaluation starts {started} instead",
        plural(slice.requested, "bar"),
        plural(warmup_need, "bar"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(days: &[&str]) -> Vec<String> {
        days.iter().map(|s| s.to_string()).collect()
    }

    /// Ten consecutive days, `2024-01-01` … `2024-01-10`.
    fn ten_days() -> Vec<String> {
        (1..=10).map(|d| format!("2024-01-{d:02}")).collect()
    }

    #[test]
    fn absent_flags_parse_to_nothing() {
        assert_eq!(DateRange::parse(None, None, false).unwrap(), None);
    }

    #[test]
    fn strict_without_from_is_refused() {
        let err = DateRange::parse(None, None, true).unwrap_err().to_string();
        assert!(err.contains("--strict-from"), "{err}");
    }

    #[test]
    fn an_unparseable_bound_names_the_flag_and_the_value() {
        let err = DateRange::parse(Some("last tuesday"), None, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("--from last tuesday"), "{err}");
    }

    #[test]
    fn until_at_or_before_from_is_refused() {
        for until in ["2024-01-01", "2023-12-31"] {
            assert!(DateRange::parse(Some("2024-01-01"), Some(until), false).is_err());
        }
    }

    /// The half-open contract, stated as the property the docs promise:
    /// splitting at a date must partition the bars exactly.
    #[test]
    fn adjacent_ranges_tile_without_overlap_or_gap() {
        let bars = ten_days();
        let dev = DateRange::parse(None, Some("2024-01-06"), false)
            .unwrap()
            .unwrap()
            .resolve(&bars, 0)
            .unwrap();
        let holdout = DateRange::parse(Some("2024-01-06"), None, true)
            .unwrap()
            .unwrap()
            .resolve(&bars, 0)
            .unwrap();
        assert_eq!(dev.eval_start..dev.end, 0..5);
        assert_eq!(holdout.eval_start..holdout.end, 5..10);
        let evaluated = (dev.end - dev.eval_start) + (holdout.end - holdout.eval_start);
        assert_eq!(evaluated, bars.len(), "every bar lands in exactly one side");
    }

    #[test]
    fn from_reads_back_exactly_the_warm_up_it_needs() {
        let bars = ten_days();
        let slice = DateRange::parse(Some("2024-01-08"), None, false)
            .unwrap()
            .unwrap()
            .resolve(&bars, 3)
            .unwrap();
        // Evaluation still begins where it was asked to...
        assert_eq!(slice.eval_start, 7);
        // ...and exactly three bars before it are fed to warm the chains.
        assert_eq!(slice.warm_start, 4);
        assert_eq!(slice.warmup_bars(), 3);
        assert_eq!(slice.fed(), 4..10);
    }

    #[test]
    fn strict_from_starts_cold_at_the_boundary() {
        let bars = ten_days();
        let slice = DateRange::parse(Some("2024-01-08"), None, true)
            .unwrap()
            .unwrap()
            .resolve(&bars, 3)
            .unwrap();
        assert_eq!(slice.warm_start, 7);
        assert_eq!(slice.eval_start, 7);
        assert_eq!(slice.warmup_bars(), 0);
    }

    /// The "warn and start late" branch: too little history before `--from`, so
    /// evaluation slips to the first settled bar rather than reporting numbers
    /// measured on a cold chain.
    #[test]
    fn insufficient_history_starts_late_rather_than_unsettled() {
        let bars = ten_days();
        let range = DateRange::parse(Some("2024-01-03"), None, false)
            .unwrap()
            .unwrap();
        // Only 2 bars precede 2024-01-03, but 5 are needed: read back to the
        // start of the series and let evaluation slip to the first settled bar.
        let slice = range.resolve(&bars, 5).unwrap();
        assert_eq!(slice.warm_start, 0, "reads back as far as the series goes");
        assert_eq!(slice.eval_start, 5, "and starts late, once settled");
        assert_eq!(slice.warmup_bars(), 5);
        assert_eq!(slice.requested, 2);

        let warn = short_warmup_warning(&slice, &bars, "2024-01-03", 5)
            .expect("evaluation slipped, so the run must say so");
        assert!(
            warn.contains("only 2 bars"),
            "counts the real history: {warn}"
        );
        assert!(warn.contains("2024-01-06"), "names the real start: {warn}");
    }

    /// The complement: enough history to warm fully, so evaluation begins
    /// exactly where it was asked to and there is nothing to warn about.
    #[test]
    fn a_full_read_back_warns_about_nothing() {
        let bars = ten_days();
        let slice = DateRange::parse(Some("2024-01-06"), None, false)
            .unwrap()
            .unwrap()
            .resolve(&bars, 5)
            .unwrap();
        assert_eq!(slice.eval_start, slice.requested);
        assert_eq!(slice.warmup_bars(), 5);
        assert!(short_warmup_warning(&slice, &bars, "2024-01-06", 5).is_none());
    }

    #[test]
    fn an_empty_selection_is_an_error_not_a_degenerate_run() {
        let bars = ten_days();
        let err = DateRange::parse(Some("2025-01-01"), None, false)
            .unwrap()
            .unwrap()
            .resolve(&bars, 0)
            .unwrap_err()
            .to_string();
        assert!(err.contains("2024-01-01 → 2024-01-10"), "{err}");
    }

    /// Bounds need not fall on a bar: `--from` snaps forward to the first bar
    /// at or after it, `--until` to the first bar at or after it (exclusive).
    #[test]
    fn bounds_between_bars_snap_to_the_enclosing_range() {
        let bars = labels(&["2024-01-01", "2024-01-10", "2024-01-20"]);
        let slice = DateRange::parse(Some("2024-01-05"), Some("2024-01-15"), true)
            .unwrap()
            .unwrap()
            .resolve(&bars, 0)
            .unwrap();
        assert_eq!(slice.eval_start..slice.end, 1..2);
    }

    /// Datetime bounds against date labels, and the reverse — the two spellings
    /// have to compare on the same axis or a slice silently shifts a day.
    #[test]
    fn date_and_datetime_spellings_agree() {
        let bars = ten_days();
        let by_date = DateRange::parse(Some("2024-01-05"), None, true)
            .unwrap()
            .unwrap()
            .resolve(&bars, 0)
            .unwrap();
        let by_stamp = DateRange::parse(Some("2024-01-05T00:00:00Z"), None, true)
            .unwrap()
            .unwrap()
            .resolve(&bars, 0)
            .unwrap();
        assert_eq!(by_date, by_stamp);
    }
}
