//! Folding irregular samples from several feeds into one atom per bar.
//!
//! A derivative's side channels do not arrive on the bar cadence anyone asks
//! for: funding settles every 4–8 hours, open interest and the positioning
//! ratios are sampled every five minutes, and the klines are the only feed that
//! is already on a grid. Whichever way those samples are obtained — Binance
//! Vision's dated CSV archives, or the live `fapi` endpoints — collapsing them
//! onto the requested interval asks the same two questions, and they have to be
//! answered the same way or the two providers disagree about the same day.
//!
//! The questions are [`Aggregation`] (does this column accrue over the bar, or
//! is it a level at a point in time?) and *which* sample a level keeps. See
//! [`Fold::sample`] for why the second one is not "the last one folded in".

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::types::{Atom, Candle, OverlayInfo, OverlayValue, Real, Schema};

use super::{Interval, Timestamp, floor_to_bucket};

/// How a column's samples collapse into one bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Aggregation {
    /// Add them up — for a quantity that accrues over the bar. The funding
    /// rate is the one that does: three 8-hourly settlements inside a day are
    /// that day's total carry.
    Sum,
    /// Keep the newest — for a quantity that is a level at a point in time.
    /// Open interest is a stock and every ratio is a proportion, so a bar
    /// keeps the number that was true when it ended.
    Last,
}

/// One bucket's value for one column, tagged with the timestamp of the sample
/// that set it.
///
/// The tag is what lets [`Aggregation::Last`] mean *newest sample* rather than
/// *last one folded in*. [`Aggregation::Sum`] doesn't consult it — addition
/// doesn't care which sample came last — but still keeps it current, so the
/// field means the same thing in every cell.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Cell {
    /// Sample timestamp, in epoch milliseconds, before bucketing.
    at: i64,
    value: Real,
}

/// The accumulator: one `bucket -> cell` map per schema column, plus the bars
/// whichever feed carries candles contributes.
///
/// Samples outside `[since, until)` are dropped on the way in, so a caller can
/// hand over whatever a feed returned without trimming it first — every feed
/// here pages by time and overshoots its window by up to one page.
#[derive(Debug)]
pub(crate) struct Fold {
    schema: Arc<Schema>,
    interval: Interval,
    since: i64,
    until: i64,
    columns: Vec<BTreeMap<i64, Cell>>,
    bars: BTreeMap<i64, Candle>,
}

impl Fold {
    /// An empty fold over `schema`'s columns, bucketing into `interval` and
    /// keeping `[since, until)`.
    pub(crate) fn new(schema: Arc<Schema>, interval: Interval, since: i64, until: i64) -> Self {
        let columns = vec![BTreeMap::new(); schema.len()];
        Self {
            schema,
            interval,
            since,
            until,
            columns,
            bars: BTreeMap::new(),
        }
    }

    /// Record a bar. The last one in a bucket wins, which for a kline feed is
    /// the same bar arriving twice — pages overlap by their boundary row.
    pub(crate) fn bar(&mut self, time: i64, candle: Candle) {
        if self.in_range(time) {
            self.bars
                .insert(floor_to_bucket(time, self.interval), candle);
        }
    }

    /// Record one sample of the column at `slot`, taken at `time`.
    ///
    /// **Levels keep the newest sample by that sample's own timestamp, not the
    /// newest write.** The two are different questions whenever a bucket is
    /// written by more than one request, which — since these feeds page, and
    /// the pages are fetched concurrently — is whenever a bucket straddles a
    /// page boundary. Taking the last writer makes the assembled series depend
    /// on what the network decided, and hands back a different answer on every
    /// run.
    pub(crate) fn sample(&mut self, time: i64, slot: usize, value: Real, agg: Aggregation) {
        if !self.in_range(time) {
            return;
        }
        let bucket = floor_to_bucket(time, self.interval);
        let cell = self.columns[slot].entry(bucket).or_insert(Cell {
            at: i64::MIN,
            value: 0.0,
        });
        match agg {
            // An accrual: samples inside one bar add up, whatever order they
            // arrive in.
            Aggregation::Sum => {
                cell.value += value;
                cell.at = cell.at.max(time);
            }
            // A level: `>=` so that two feeds carrying the same instant
            // resolve to the later of them in the caller's fold order, which
            // the caller fixes deterministically.
            Aggregation::Last => {
                if time >= cell.at {
                    *cell = Cell { at: time, value };
                }
            }
        }
    }

    /// One [`Atom`] per bucket any feed reached, ascending by time.
    ///
    /// A bucket carries a candle only when the bar feed covered it, and each
    /// overlay column only where a sample landed: the feeds start at different
    /// dates and run at different cadences, and an absent column has to read as
    /// an absent sample rather than as a zero — for an accrual that is the
    /// difference between "no carry recorded" and "carry was nil".
    pub(crate) fn finish(self) -> Vec<Atom> {
        let mut buckets: Vec<i64> = self
            .columns
            .iter()
            .flat_map(|c| c.keys().copied())
            .chain(self.bars.keys().copied())
            .collect();
        buckets.sort_unstable();
        buckets.dedup();

        buckets
            .into_iter()
            .map(|time| Atom {
                candle: self.bars.get(&time).copied(),
                time: Some(Timestamp(time)),
                overlays: Some(OverlayInfo::sparse(
                    self.schema.clone(),
                    self.columns
                        .iter()
                        .map(|c| c.get(&time).map(|cell| OverlayValue::Real(cell.value))),
                )),
            })
            .collect()
    }

    fn in_range(&self, time: i64) -> bool {
        time >= self.since && time < self.until
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema() -> Arc<Schema> {
        let mut b = Schema::builder();
        b.add_real("accrual");
        b.add_real("level");
        b.finish()
    }

    const DAY: i64 = 86_400_000;

    #[test]
    fn accruals_add_and_levels_keep_the_newest_sample() {
        let mut fold = Fold::new(schema(), Interval::Day(1), 0, 2 * DAY);
        fold.sample(DAY, 0, 1.0, Aggregation::Sum);
        fold.sample(DAY + 3_600_000, 0, 2.0, Aggregation::Sum);
        // The level arrives newest-first, so a fold that kept the last write
        // would answer 5.0 here.
        fold.sample(DAY + 7_200_000, 1, 9.0, Aggregation::Last);
        fold.sample(DAY + 60_000, 1, 5.0, Aggregation::Last);
        let atoms = fold.finish();
        assert_eq!(atoms.len(), 1);
        let overlays = atoms[0].overlays.as_ref().expect("bound");
        assert_eq!(
            overlays.get_by_key("accrual"),
            Some(&OverlayValue::Real(3.0))
        );
        assert_eq!(overlays.get_by_key("level"), Some(&OverlayValue::Real(9.0)));
    }

    #[test]
    fn samples_outside_the_window_are_dropped() {
        let mut fold = Fold::new(schema(), Interval::Day(1), DAY, 2 * DAY);
        fold.sample(0, 0, 1.0, Aggregation::Sum); // before `since`
        fold.sample(2 * DAY, 0, 1.0, Aggregation::Sum); // at `until`, exclusive
        fold.bar(0, Candle::new(1.0, 1.0, 1.0, 1.0, 1.0));
        assert!(fold.finish().is_empty());
    }

    #[test]
    fn a_bucket_only_an_overlay_reached_carries_no_candle() {
        let mut fold = Fold::new(schema(), Interval::Day(1), 0, 2 * DAY);
        fold.bar(0, Candle::new(1.0, 2.0, 0.5, 1.5, 10.0));
        fold.sample(DAY, 1, 4.0, Aggregation::Last);
        let atoms = fold.finish();
        assert_eq!(atoms.len(), 2);
        assert_eq!(atoms[0].candle.expect("bar bucket").close, 1.5);
        assert!(atoms[1].candle.is_none());
        // …and the bar's own bucket has no level sample, rather than a zero.
        let overlays = atoms[0].overlays.as_ref().expect("bound");
        assert_eq!(overlays.get_by_key("level"), None);
    }
}
