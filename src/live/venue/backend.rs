//! What a live backend supplies so the shared [`Wallet`] bodies in
//! [`flow`](super::flow) can drive it.

use crate::types::{Real, Symbol};
use crate::wallet::{OrderId, OrderKind, Side, Wallet};

use super::super::LiveError;
use super::{CursorModel, InstrumentGrid, LiveCore, OrderClass, VenueFill};

/// A live venue's own half: its endpoints, its envelopes, and its request
/// bodies.
///
/// **Every item here is a venue fact.** None of it is an algorithm — those live
/// in [`flow`](super::flow) and are the same for every venue by construction,
/// which is the whole point. If writing a `flow` body ever calls for a hook
/// whose implementation is `true`, `false`, `unreachable!()` or an empty body
/// on one venue, that method belongs back in the venue file rather than here.
///
/// **Not part of the public [`Wallet`] trait, and must never become part of
/// it.** Capability on `Wallet` is expressed by overriding-or-defaulting on one
/// trait; this is an internal implementation-sharing trait, visible only inside
/// `crate::live`, and it changes nothing a downstream implementor of `Wallet`
/// sees.
///
/// **Every hook returns [`LiveError`], never
/// [`WalletError`](crate::WalletError).** Choosing between "log it"
/// ([`LiveLog::fail`](super::LiveLog::fail)) and "log it and buffer a
/// `Rejection`" ([`LiveLog::refuse`](super::LiveLog::refuse)) is a property of
/// the *call site*, and every call site is in `flow`.
///
/// **`flow` may read only five `Wallet` methods** — `funds`, `equity`, `price`,
/// `position` and `can_short` — because those are the five neither wallet
/// delegates back to `flow`. Calling any other would recurse forever.
pub(in crate::live) trait VenueBackend: Wallet<Symbol> {
    // --- shared state ------------------------------------------------------

    fn core(&self) -> &LiveCore;
    fn core_mut(&mut self) -> &mut LiveCore;

    // --- account -----------------------------------------------------------

    /// Re-read balances and positions from the venue into this wallet's own
    /// cache. Per-venue because the endpoints, the pagination and the very
    /// meaning of "position" all differ — a signed swap position on one venue,
    /// a table of currency balances on the other.
    fn refresh(&mut self) -> Result<(), LiveError>;

    // --- instrument grid ---------------------------------------------------

    /// Fetch (uncached) the trading grid for `symbol`.
    /// [`flow::ensure_grid`](super::flow::ensure_grid) owns the caching.
    fn fetch_grid(&mut self, symbol: &str) -> Result<InstrumentGrid, LiveError>;

    // --- fills -------------------------------------------------------------

    /// Which dedupe model this venue's fill feed supports.
    fn cursor_model(&self) -> CursorModel;

    /// The venue's recent fills for `symbol`, normalized. `flow` orders,
    /// filters and converts them.
    fn fetch_fills(&mut self, symbol: &str) -> Result<Vec<VenueFill>, LiveError>;

    // --- submission --------------------------------------------------------

    /// Place a market order for `size` **venue-native** units, returning the
    /// venue order id. `local` is the minted id to tag as the client order id,
    /// so a later fill poll can correlate.
    fn place_market(
        &mut self,
        symbol: &str,
        side: Side,
        size: Real,
        grid: &InstrumentGrid,
        local: OrderId,
    ) -> Result<String, LiveError>;

    /// Place a resting limit order at `price`, already snapped to the grid's
    /// tick by the caller.
    fn place_limit(
        &mut self,
        symbol: &str,
        side: Side,
        size: Real,
        price: Real,
        grid: &InstrumentGrid,
        local: OrderId,
    ) -> Result<String, LiveError>;

    /// Place a **reduce-only** protective leg. `kind` is `Stop` or
    /// `TakeProfit`; `side` is the exit side `flow` derived from the open
    /// position; `trigger` is already on the tick.
    #[allow(clippy::too_many_arguments)]
    fn place_protective(
        &mut self,
        symbol: &str,
        kind: OrderKind,
        side: Side,
        size: Real,
        trigger: Real,
        grid: &InstrumentGrid,
        local: OrderId,
    ) -> Result<String, LiveError>;

    /// Cancel a venue order, treating "already gone" as success — the
    /// post-condition holds either way. `class` picks the endpoint on a venue
    /// with more than one.
    fn cancel_venue_order(
        &mut self,
        symbol: &str,
        venue_id: &str,
        class: OrderClass,
    ) -> Result<(), LiveError>;
}
