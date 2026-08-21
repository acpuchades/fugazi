//! The normalized fill feed: one fill shape, and the two ways a venue lets you
//! tell a fill you have already reported from one you haven't.
//!
//! Both backends poll a *recent window* endpoint every bar rather than a
//! since-cursor one — robust at the once-per-bar cadence a driver uses, but it
//! means every poll re-reads fills already delivered. Deduping is therefore not
//! an optimisation; without it the strategy would see the same fill on every
//! bar for as long as the window holds it.

use std::collections::{HashSet, VecDeque};

use crate::types::Real;
use crate::wallet::Side;

/// One venue fill, normalized.
///
/// `size` is **venue-native** (contracts on a derivatives venue, base units on
/// spot); the caller multiplies by the instrument grid's contract multiplier on
/// the way out, so nothing above the wallet ever sees a contract.
#[derive(Debug, Clone)]
pub(in crate::live) struct VenueFill {
    /// The venue's monotone integer key, where it has one (OKX's `billId`).
    /// `None` on a venue that only issues opaque ids.
    pub(in crate::live) ordinal: Option<i64>,
    /// The per-fill identity every venue has.
    pub(in crate::live) id: String,
    /// The ordering key to use when there is no [`ordinal`](Self::ordinal).
    pub(in crate::live) sequence: String,
    /// The venue order this fill executed against.
    pub(in crate::live) order_id: String,
    pub(in crate::live) side: Side,
    pub(in crate::live) size: Real,
    pub(in crate::live) price: Real,
    pub(in crate::live) commission: Real,
}

impl VenueFill {
    /// One ordering for both cursor models, oldest first, so partial fills
    /// reach the strategy in execution order.
    ///
    /// A venue with an ordinal sorts by it; one without sorts by `sequence`
    /// alone, since every fill then reports `ordinal: None` and the first term
    /// is constant.
    pub(in crate::live) fn sort_key(&self) -> (i64, &str) {
        (self.ordinal.unwrap_or(0), &self.sequence)
    }
}

/// Which dedupe model a venue's fill feed supports.
#[derive(Debug, Clone, Copy)]
pub(in crate::live) enum CursorModel {
    /// The feed carries a monotone integer key, so "already reported" is a
    /// single high-water mark. O(1) memory.
    Watermark,
    /// The feed carries only opaque ids, so already-reported fills have to be
    /// remembered individually.
    ///
    /// `capacity` must comfortably exceed the largest page the fills endpoint
    /// returns, or an evicted id could still appear in a poll and be re-reported
    /// as new.
    SeenIds { capacity: usize },
}

/// Per-symbol fill dedupe state.
#[derive(Debug)]
pub(in crate::live) enum FillCursor {
    Watermark(i64),
    Seen(SeenFills),
}

impl FillCursor {
    /// Seed from the fills already on the venue, so we only ever report fills
    /// that happen *after* we started trading a symbol — not the account's
    /// whole history.
    pub(in crate::live) fn seeded(model: CursorModel, fills: &[VenueFill]) -> Self {
        match model {
            CursorModel::Watermark => {
                FillCursor::Watermark(fills.iter().filter_map(|f| f.ordinal).max().unwrap_or(0))
            }
            CursorModel::SeenIds { capacity } => {
                let mut seen = SeenFills::new(capacity);
                for fill in fills {
                    seen.insert(&fill.id);
                }
                FillCursor::Seen(seen)
            }
        }
    }

    /// Whether `fill` has not been reported yet — recording it either way, so a
    /// later poll returning the same fill is refused.
    pub(in crate::live) fn admit(&mut self, fill: &VenueFill) -> bool {
        match self {
            FillCursor::Watermark(mark) => {
                let ordinal = fill.ordinal.unwrap_or(0);
                if ordinal <= *mark {
                    return false;
                }
                *mark = ordinal;
                true
            }
            FillCursor::Seen(seen) => seen.insert(&fill.id),
        }
    }
}

/// A FIFO-bounded set of already-reported fill ids.
///
/// The unbounded `HashSet` this replaces held one entry per fill for the
/// lifetime of the process, in the code path that runs most often. Evicting the
/// oldest is safe because the venue's fills endpoint returns a bounded *recent*
/// window: an id old enough to be evicted can no longer come back in a poll.
#[derive(Debug)]
pub(in crate::live) struct SeenFills {
    set: HashSet<String>,
    order: VecDeque<String>,
    capacity: usize,
}

impl SeenFills {
    fn new(capacity: usize) -> Self {
        Self {
            set: HashSet::new(),
            order: VecDeque::new(),
            capacity: capacity.max(1),
        }
    }

    /// `true` if `id` was not already present.
    fn insert(&mut self, id: &str) -> bool {
        if !self.set.insert(id.to_string()) {
            return false;
        }
        self.order.push_back(id.to_string());
        while self.order.len() > self.capacity {
            if let Some(evicted) = self.order.pop_front() {
                self.set.remove(&evicted);
            }
        }
        true
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.set.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fill(ordinal: Option<i64>, id: &str) -> VenueFill {
        VenueFill {
            ordinal,
            id: id.to_string(),
            sequence: id.to_string(),
            order_id: "o".into(),
            side: Side::Buy,
            size: 1.0,
            price: 100.0,
            commission: 0.0,
        }
    }

    /// A watermark seeds to the highest existing ordinal, so the account's
    /// pre-existing history is never replayed to the strategy as fresh fills.
    #[test]
    fn a_watermark_cursor_seeds_past_the_existing_history() {
        let history = [fill(Some(5), "a"), fill(Some(9), "b"), fill(Some(7), "c")];
        let mut cursor = FillCursor::seeded(CursorModel::Watermark, &history);
        assert!(!cursor.admit(&fill(Some(9), "b")), "seeded fills are stale");
        assert!(cursor.admit(&fill(Some(10), "d")), "a newer fill is fresh");
        assert!(!cursor.admit(&fill(Some(10), "d")), "and only once");
    }

    /// Same contract for a venue with no ordinal.
    #[test]
    fn a_seen_id_cursor_seeds_past_the_existing_history() {
        let history = [fill(None, "a"), fill(None, "b")];
        let mut cursor = FillCursor::seeded(CursorModel::SeenIds { capacity: 8 }, &history);
        assert!(!cursor.admit(&fill(None, "b")));
        assert!(cursor.admit(&fill(None, "z")));
        assert!(!cursor.admit(&fill(None, "z")));
    }

    /// The seen-id set is bounded, and still dedupes everything *inside* the
    /// window — which is all that matters, because the fills endpoint only ever
    /// returns a bounded recent page.
    #[test]
    fn the_seen_id_set_is_bounded_and_still_dedupes_within_its_window() {
        const CAP: usize = 16;
        let mut seen = SeenFills::new(CAP);
        for i in 0..CAP * 4 {
            assert!(
                seen.insert(&format!("t{i}")),
                "each id is new the first time"
            );
            assert!(seen.len() <= CAP, "the set grew past its capacity");
        }
        // Everything still in the window is deduped...
        for i in CAP * 3..CAP * 4 {
            assert!(!seen.insert(&format!("t{i}")), "a recent id is still known");
        }
        // ...and only what fell out of it is forgotten, which the venue can no
        // longer hand back.
        assert!(seen.insert("t0"), "an evicted id is no longer remembered");
    }

    /// Sorting is oldest-first under both models, so partial fills reach the
    /// strategy in execution order.
    #[test]
    fn fills_sort_oldest_first_under_both_models() {
        let mut with_ordinal = [fill(Some(3), "c"), fill(Some(1), "a"), fill(Some(2), "b")];
        with_ordinal.sort_by(|x, y| x.sort_key().cmp(&y.sort_key()));
        assert_eq!(
            with_ordinal
                .iter()
                .map(|f| f.id.as_str())
                .collect::<Vec<_>>(),
            ["a", "b", "c"]
        );

        let mut without = [fill(None, "c"), fill(None, "a"), fill(None, "b")];
        without.sort_by(|x, y| x.sort_key().cmp(&y.sort_key()));
        assert_eq!(
            without.iter().map(|f| f.id.as_str()).collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
    }
}
