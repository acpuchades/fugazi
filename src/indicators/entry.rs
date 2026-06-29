//! Position-anchored sources: the entry price of the current position and the
//! extremes reached since entry.
//!
//! Unlike every other source, these are not pure functions of the candle stream
//! — they depend on *when a position was opened*, which only the decision layer
//! knows. They read that one fact from a shared [`EntryAnchor`] the strategy
//! owns and arms on entry, so a stop-loss / take-profit level is expressed as an
//! ordinary indicator expression: `Entry::new(anchor).sub(Atr::new(14).mul(...))`
//! for a fixed stop, `PeakSinceEntry::new(anchor).mul(Value::new(0.95))` for a
//! trailing one. They read `None` (not ready) while flat.

use std::cell::Cell;
use std::marker::PhantomData;
use std::rc::Rc;

use crate::indicator::Indicator;
use crate::types::{Candle, Real};

/// A shared, mutable handle to the current position's entry price — `None` while
/// flat, `Some(price)` once a position is open.
///
/// A strategy (e.g. [`SingleAssetStrategy`](crate::strategies::SingleAssetStrategy))
/// owns one anchor and hands clones to the position-anchored sources built into
/// its stop levels; on entry it [`arm`](EntryAnchor::arm)s the anchor with the
/// fill price and on exit [`clear`](EntryAnchor::clear)s it. Backed by an
/// `Rc<Cell<…>>`, so cloning shares one anchor and a strategy can set it through
/// `&self` from its price-free `trade`.
#[derive(Debug, Clone, Default)]
pub struct EntryAnchor(Rc<Cell<Option<Real>>>);

impl EntryAnchor {
    /// A fresh anchor in the flat (empty) state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that a position opened at `price`.
    pub fn arm(&self, price: Real) {
        self.0.set(Some(price));
    }

    /// Record that the position was flattened.
    pub fn clear(&self) {
        self.0.set(None);
    }

    /// The current entry price, or `None` while flat.
    pub fn get(&self) -> Option<Real> {
        self.0.get()
    }
}

/// The entry price of the current position, `None` while flat.
///
/// A leaf source wired to a strategy's [`EntryAnchor`]; the building block of a
/// fixed stop-loss / take-profit level.
#[derive(Debug, Clone)]
pub struct Entry {
    anchor: EntryAnchor,
}

impl Entry {
    /// An `Entry` reading from `anchor`.
    pub fn new(anchor: EntryAnchor) -> Self {
        Self { anchor }
    }
}

impl Indicator for Entry {
    type Input = Candle;
    type Output = Real;

    fn update(&mut self, _candle: Candle) -> Option<Real> {
        self.anchor.get()
    }

    fn value(&self) -> Option<Real> {
        self.anchor.get()
    }

    fn reset(&mut self) {}
}

/// Selects the bar field and reduction a [`SinceEntry`] accumulates.
trait SinceEntryOp {
    /// The bar field that moves the extreme (high for a peak, low for a trough).
    fn pick(candle: &Candle) -> Real;
    /// Fold the running extreme with the latest pick (max for a peak, min for a
    /// trough).
    fn merge(running: Real, latest: Real) -> Real;
}

/// Running maximum: tracks the bar `high`.
#[derive(Debug, Clone, Copy)]
pub struct PeakOp;
impl SinceEntryOp for PeakOp {
    fn pick(candle: &Candle) -> Real {
        candle.high
    }
    fn merge(running: Real, latest: Real) -> Real {
        running.max(latest)
    }
}

/// Running minimum: tracks the bar `low`.
#[derive(Debug, Clone, Copy)]
pub struct TroughOp;
impl SinceEntryOp for TroughOp {
    fn pick(candle: &Candle) -> Real {
        candle.low
    }
    fn merge(running: Real, latest: Real) -> Real {
        running.min(latest)
    }
}

/// The extreme price reached over the **completed** bars since the current
/// position opened (`None` while flat), seeded at the entry price and restarted
/// whenever the anchor changes (a new entry, including a reversal).
///
/// It deliberately **excludes the bar in progress**: the value reported on a bar
/// is the extreme through the *previous* one. For a trailing stop that means the
/// level was already fixed at the bar's open (so a fill can tell a real gap from
/// intra-bar movement), and the stop reacts on the bar after a new extreme — not
/// the same bar, which would be intra-bar look-ahead.
///
/// Use the aliases [`PeakSinceEntry`] (running high, for a long trailing stop)
/// and [`TroughSinceEntry`] (running low, for a short trailing stop).
#[derive(Debug, Clone)]
pub struct SinceEntry<Op> {
    anchor: EntryAnchor,
    /// The extreme over *completed* bars (what `value` reports) — excludes the
    /// bar in progress, so a level built on it was already known at this bar's
    /// open (which is what lets a stop tell a real gap from intra-bar movement).
    reported: Option<Real>,
    /// The extreme including the current bar, carried into the next one.
    running: Option<Real>,
    last_entry: Option<Real>,
    _op: PhantomData<fn() -> Op>,
}

impl<Op> SinceEntry<Op> {
    /// A `SinceEntry` reading from `anchor`.
    pub fn new(anchor: EntryAnchor) -> Self {
        Self {
            anchor,
            reported: None,
            running: None,
            last_entry: None,
            _op: PhantomData,
        }
    }
}

impl<Op: SinceEntryOp> Indicator for SinceEntry<Op> {
    type Input = Candle;
    type Output = Real;

    fn update(&mut self, candle: Candle) -> Option<Real> {
        let entry = self.anchor.get();
        // A changed anchor (open, close, or reversal) restarts at the new entry
        // price — `None` when the change is a flatten.
        if entry != self.last_entry {
            self.last_entry = entry;
            self.running = entry;
        }
        // Report the extreme as of the *prior* bars (the running value before
        // folding in this bar), then fold this bar in for next time.
        self.reported = self.running;
        if entry.is_some() {
            let latest = Op::pick(&candle);
            let base = self.running.unwrap_or(latest);
            self.running = Some(Op::merge(base, latest));
        }
        self.reported
    }

    fn value(&self) -> Option<Real> {
        self.reported
    }

    fn reset(&mut self) {
        self.reported = None;
        self.running = None;
        self.last_entry = None;
    }
}

/// The running high since the current position opened — a long trailing stop's
/// anchor (`PeakSinceEntry::new(anchor).mul(Value::new(1.0 - frac))`).
pub type PeakSinceEntry = SinceEntry<PeakOp>;
/// The running low since the current position opened — a short trailing stop's
/// anchor.
pub type TroughSinceEntry = SinceEntry<TroughOp>;

#[cfg(test)]
mod tests {
    use super::*;

    fn bar(high: Real, low: Real) -> Candle {
        Candle::new((high + low) / 2.0, high, low, (high + low) / 2.0, 0.0)
    }

    #[test]
    fn entry_is_none_until_armed() {
        let anchor = EntryAnchor::new();
        let mut e = Entry::new(anchor.clone());
        assert_eq!(e.update(bar(10.0, 9.0)), None);
        anchor.arm(9.5);
        assert_eq!(e.update(bar(10.0, 9.0)), Some(9.5));
        anchor.clear();
        assert_eq!(e.update(bar(10.0, 9.0)), None);
    }

    #[test]
    fn peak_tracks_high_over_completed_bars_and_restarts_on_new_entry() {
        let anchor = EntryAnchor::new();
        let mut peak = PeakSinceEntry::new(anchor.clone());
        // Flat: not ready.
        assert_eq!(peak.update(bar(10.0, 9.0)), None);
        // Enter at 9.5; the first armed bar reports the entry (no completed bar
        // contributes a high yet).
        anchor.arm(9.5);
        assert_eq!(peak.update(bar(11.0, 9.0)), Some(9.5));
        // Now the prior bar's high (11) is reflected; this bar's high is excluded.
        assert_eq!(peak.update(bar(12.0, 9.5)), Some(11.0));
        assert_eq!(peak.update(bar(10.5, 9.5)), Some(12.0)); // holds the prior peak
        // Reverse into a new position at 10.0: the peak restarts at the entry.
        anchor.arm(10.0);
        assert_eq!(peak.update(bar(10.2, 9.8)), Some(10.0));
    }

    #[test]
    fn trough_tracks_low_over_completed_bars() {
        let anchor = EntryAnchor::new();
        let mut trough = TroughSinceEntry::new(anchor.clone());
        anchor.arm(20.0);
        assert_eq!(trough.update(bar(21.0, 19.0)), Some(20.0)); // entry only
        assert_eq!(trough.update(bar(20.5, 18.0)), Some(19.0)); // prior bar's low
        assert_eq!(trough.update(bar(20.0, 17.0)), Some(18.0)); // holds, excl current
    }
}
