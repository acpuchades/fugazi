//! [`Position`]: a strategy's own view of its open position, and the
//! position-anchored sources built from it (entry price, and the extremes since
//! entry).
//!
//! Unlike every other source, these are not pure functions of the candle stream
//! — they depend on *when a position was opened and at what price*, which only
//! the decision layer knows. A strategy owns one `Position`, updates it from its
//! fills ([`apply`](Position::apply)) and each bar ([`update`](Position::update)),
//! and reads its side/size to decide. Its [`entry`](Position::entry) /
//! [`peak`](Position::peak) / [`trough`](Position::trough) accessors expose those
//! facts as ordinary [`Indicator`]s, so a stop-loss / take-profit *level* is just
//! an expression: `position.entry().sub(Atr::new(14).mul(...))` for a fixed stop,
//! `position.peak().mul(Value::new(0.95))` for a trailing one. They read `None`
//! (not ready) while flat.

use std::cell::RefCell;
use std::rc::Rc;

use crate::indicator::Indicator;
use crate::indicators::DEFAULT_EPSILON;
use crate::strategy::Side;
use crate::types::{Atom, Candle, Real};

/// The running state a [`Position`] shares.
#[derive(Debug, Default)]
struct PositionState {
    /// Signed position size (positive long, negative short, zero flat).
    size: Real,
    /// Entry price of the current position, `None` while flat.
    entry: Option<Real>,
    /// Highest `high` over the **completed** bars since entry (what [`peak`] reports).
    ///
    /// [`peak`]: Position::peak
    peak: Option<Real>,
    /// Lowest `low` over the completed bars since entry (what [`trough`] reports).
    ///
    /// [`trough`]: Position::trough
    trough: Option<Real>,
    /// Running high including the bar in progress, carried into the next bar.
    peak_running: Option<Real>,
    /// Running low including the bar in progress.
    trough_running: Option<Real>,
}

/// A strategy's own view of its open position — signed size, entry price, and the
/// extremes reached since entry — updated from its [`fills`](Position::apply) and
/// each [`bar`](Position::update).
///
/// A strategy (e.g. [`SingleAssetStrategy`](crate::strategies::SingleAssetStrategy))
/// owns one `Position` and hands clones to the position-anchored sources built
/// into its stop levels. Backed by an `Rc<RefCell<…>>`, so cloning shares one
/// state and the level sources read the same facts the strategy writes.
#[derive(Debug, Clone, Default)]
pub struct Position(Rc<RefCell<PositionState>>);

impl Position {
    /// A fresh, flat position.
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply a fill of `units` at `price` on `side` — the strategy's link from the
    /// wallet's fill stream to its own position view. Opening from flat or reversing
    /// through zero re-anchors the entry at `price` and restarts the extremes;
    /// flattening clears everything; scaling the same side keeps the entry.
    pub fn apply(&self, side: Side, units: Real, price: Real) {
        let mut s = self.0.borrow_mut();
        let new_size = s.size + side.sign() * units;
        let crossed_zero = s.size * new_size < 0.0;
        if new_size.abs() <= DEFAULT_EPSILON {
            *s = PositionState::default();
        } else if s.entry.is_none() || crossed_zero {
            s.size = new_size;
            s.entry = Some(price);
            s.peak = None;
            s.trough = None;
            s.peak_running = Some(price);
            s.trough_running = Some(price);
        } else {
            s.size = new_size;
        }
    }

    /// Fold `candle` into the extremes since entry. Publishes the extreme over the
    /// **completed** bars (excluding the bar in progress) then folds this bar in,
    /// so a trailing level built on [`peak`](Position::peak) is already fixed at the
    /// bar's open and reacts on the bar *after* a new extreme — never intra-bar.
    /// A no-op while flat.
    pub fn update(&self, candle: Candle) {
        let mut s = self.0.borrow_mut();
        if s.entry.is_none() {
            return;
        }
        s.peak = s.peak_running;
        s.trough = s.trough_running;
        s.peak_running = Some(s.peak_running.map_or(candle.high, |p| p.max(candle.high)));
        s.trough_running = Some(s.trough_running.map_or(candle.low, |t| t.min(candle.low)));
    }

    /// Reset to flat.
    pub fn reset(&self) {
        *self.0.borrow_mut() = PositionState::default();
    }

    /// The signed position size (positive long, negative short, zero flat).
    pub fn size(&self) -> Real {
        self.0.borrow().size
    }

    /// Whether the position is meaningfully long.
    pub fn is_long(&self) -> bool {
        self.size() > DEFAULT_EPSILON
    }

    /// Whether the position is meaningfully short.
    pub fn is_short(&self) -> bool {
        self.size() < -DEFAULT_EPSILON
    }

    /// Whether the position is flat.
    pub fn is_flat(&self) -> bool {
        self.size().abs() <= DEFAULT_EPSILON
    }

    /// The entry price of the current position, `None` while flat.
    pub fn entry_price(&self) -> Option<Real> {
        self.0.borrow().entry
    }

    /// The entry price as an [`Indicator`] — the leaf of a fixed stop / take-profit
    /// level. `None` while flat.
    pub fn entry(&self) -> PositionField {
        PositionField::new(self.clone(), |s| s.entry)
    }

    /// The running high since entry (completed bars) as an [`Indicator`] — the leaf
    /// of a long trailing stop. `None` while flat.
    pub fn peak(&self) -> PositionField {
        PositionField::new(self.clone(), |s| s.peak)
    }

    /// The running low since entry (completed bars) as an [`Indicator`] — the leaf
    /// of a short trailing stop. `None` while flat.
    pub fn trough(&self) -> PositionField {
        PositionField::new(self.clone(), |s| s.trough)
    }
}

/// One field of a shared [`Position`], projected into an
/// `Indicator<Input = Atom, Output = Real>` so a stop / take-profit level
/// composes like any other source. Returned by [`Position::entry`] /
/// [`peak`](Position::peak) / [`trough`](Position::trough); reads live state and
/// ignores its input (the owning [`Position`] is advanced by the strategy).
#[derive(Debug, Clone)]
pub struct PositionField {
    position: Position,
    select: fn(&PositionStateRef) -> Option<Real>,
}

/// The borrow a [`PositionField`] selector reads from (a private alias so the
/// selector `fn` can name the state type without exposing it).
type PositionStateRef = PositionState;

impl PositionField {
    fn new(position: Position, select: fn(&PositionStateRef) -> Option<Real>) -> Self {
        Self { position, select }
    }
}

impl Indicator for PositionField {
    type Input = Atom;
    type Output = Real;

    fn update(&mut self, _atom: Atom) -> Option<Real> {
        self.value()
    }

    fn value(&self) -> Option<Real> {
        (self.select)(&self.position.0.borrow())
    }

    /// `0`: readiness tracks the live [`Position`] (open vs flat), not how many
    /// samples this field has seen.
    fn warm_up_period(&self) -> usize {
        0
    }

    fn reset(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar(high: Real, low: Real) -> Candle {
        Candle::new((high + low) / 2.0, high, low, (high + low) / 2.0, 0.0)
    }

    #[test]
    fn entry_is_none_until_a_fill_opens_the_position() {
        let pos = Position::new();
        let mut e = pos.entry();
        assert_eq!(e.update(bar(10.0, 9.0).into()), None);
        pos.apply(Side::Buy, 1.0, 9.5);
        assert_eq!(e.update(bar(10.0, 9.0).into()), Some(9.5));
        // Selling it back to flat clears the entry.
        pos.apply(Side::Sell, 1.0, 10.0);
        assert_eq!(e.update(bar(10.0, 9.0).into()), None);
    }

    #[test]
    fn peak_tracks_high_over_completed_bars_and_restarts_on_reversal() {
        let pos = Position::new();
        let peak = pos.peak();
        // Flat: not ready.
        assert_eq!(peak.value(), None);
        // Enter long at 9.5; the first bar reports the entry (no completed bar yet).
        pos.apply(Side::Buy, 1.0, 9.5);
        pos.update(bar(11.0, 9.0));
        assert_eq!(peak.value(), Some(9.5));
        // Now the prior bar's high (11) is reflected; this bar's high is excluded.
        pos.update(bar(12.0, 9.5));
        assert_eq!(peak.value(), Some(11.0));
        pos.update(bar(10.5, 9.5));
        assert_eq!(peak.value(), Some(12.0)); // holds the prior peak
        // Reverse into a short at 10.0 (sell 2 flips +1 -> -1): peak restarts.
        pos.apply(Side::Sell, 2.0, 10.0);
        pos.update(bar(10.2, 9.8));
        assert_eq!(peak.value(), Some(10.0));
    }

    #[test]
    fn trough_tracks_low_over_completed_bars() {
        let pos = Position::new();
        let trough = pos.trough();
        pos.apply(Side::Sell, 1.0, 20.0);
        pos.update(bar(21.0, 19.0));
        assert_eq!(trough.value(), Some(20.0)); // entry only
        pos.update(bar(20.5, 18.0));
        assert_eq!(trough.value(), Some(19.0)); // prior bar's low
        pos.update(bar(20.0, 17.0));
        assert_eq!(trough.value(), Some(18.0)); // holds, excludes current
    }

    #[test]
    fn tracks_side_and_size() {
        let pos = Position::new();
        assert!(pos.is_flat());
        pos.apply(Side::Buy, 3.0, 100.0);
        assert!(pos.is_long());
        assert_eq!(pos.size(), 3.0);
        assert_eq!(pos.entry_price(), Some(100.0));
        pos.apply(Side::Sell, 5.0, 110.0); // flip to short 2
        assert!(pos.is_short());
        assert_eq!(pos.size(), -2.0);
        assert_eq!(pos.entry_price(), Some(110.0));
    }
}
