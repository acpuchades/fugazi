//! Backwards-compatible facade over the split type modules.
//!
//! The core scalar and market-data vocabulary previously bundled here now
//! lives in three narrower modules:
//!
//! * [`crate::time`] — [`Timestamp`] and [`Frequency`].
//! * [`crate::market`] — [`Real`], [`Candle`], [`OverlayType`] / [`OverlayValue`],
//!   [`Schema`] / [`SchemaBuilder`], [`OverlayInfo`], and [`Atom`].
//! * [`crate::snapshot`] — [`Selector`] and [`Snapshot`].
//!
//! Every name still re-exports from this module, so existing `fugazi::types::*`
//! import paths keep working.

pub use crate::market::{
    Atom, Candle, OverlayInfo, OverlayType, OverlayValue, Real, Schema, SchemaBuilder,
};
pub use crate::snapshot::{Selector, Snapshot};
pub use crate::time::{Frequency, Timestamp};
