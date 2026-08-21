//! Bookkeeping every live backend keeps, independent of the venue.

use crate::hash::SymMap;
use crate::types::{Real, Symbol};
use crate::wallet::{
    DEFAULT_RETENTION, OrderId, OrderKind, POSITION_EPSILON, Rejection, Side, WalletError,
    trim_front,
};

use super::super::LiveError;
use super::{FillCursor, HttpCore, floor_to_step, format_decimals, round_to_tick};

/// Which cancel endpoint a venue order belongs to.
///
/// OKX has two (`/trade/cancel-order` for a resting entry,
/// `/trade/cancel-algos` for a protective leg); a venue with one ignores this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::live) enum OrderClass {
    /// A resting limit entry.
    Entry,
    /// A stop or take-profit leg.
    Protective,
}

/// The trading grid for one instrument, in **venue-native size units** —
/// contracts on a derivatives venue, base-asset units on spot.
///
/// One type for what the two backends called `InstrumentSpec` and
/// `ProductSpec`: the same five concepts under different names, plus OKX's
/// `ctVal`. `contract_multiplier` is that factor — base units = venue size ×
/// multiplier — and a spot venue reports `1.0`, at which every conversion below
/// collapses to the identity *exactly* (multiplying and dividing by `1.0` is
/// lossless in IEEE 754, so adopting this changed no spot arithmetic).
///
/// Fetched once per symbol and cached; the caller owns the caching.
#[derive(Debug, Clone, Copy)]
pub(in crate::live) struct InstrumentGrid {
    /// Size step, in venue-native units.
    pub(in crate::live) size_step: Real,
    /// Minimum order size, in venue-native units.
    pub(in crate::live) min_size: Real,
    /// Price step.
    pub(in crate::live) price_tick: Real,
    /// Base-asset value of one venue-native size unit. `1.0` on spot.
    pub(in crate::live) contract_multiplier: Real,
    pub(in crate::live) size_decimals: usize,
    pub(in crate::live) price_decimals: usize,
}

impl InstrumentGrid {
    /// Base units → the venue-native size to submit: divide out the contract
    /// multiplier, then floor to the step, so a rounded order is never *larger*
    /// than the diff it was meant to close.
    pub(in crate::live) fn venue_size(&self, base_units: Real) -> Real {
        floor_to_step(base_units / self.contract_multiplier, self.size_step)
    }

    /// Venue-native size → base units. Nothing above the wallet ever sees a
    /// contract, so every fill goes through here on the way out.
    pub(in crate::live) fn base_units(&self, venue_size: Real) -> Real {
        venue_size * self.contract_multiplier
    }

    /// A price snapped to the venue's tick.
    pub(in crate::live) fn on_tick(&self, price: Real) -> Real {
        round_to_tick(price, self.price_tick)
    }

    pub(in crate::live) fn size_str(&self, venue_size: Real) -> String {
        format_decimals(venue_size, self.size_decimals)
    }

    pub(in crate::live) fn price_str(&self, price: Real) -> String {
        format_decimals(price, self.price_decimals)
    }

    /// Whether `venue_size` is below what the venue will accept — either under
    /// its stated minimum or rounded away to nothing.
    pub(in crate::live) fn below_minimum(&self, venue_size: Real) -> bool {
        venue_size < self.min_size || venue_size <= POSITION_EPSILON
    }
}

/// Wallet-minted local [`OrderId`]s ↔ venue order ids, plus the [`OrderKind`]
/// each venue order was placed as, so a polled fill can be tagged.
///
/// A venue reports a fill against *its* id; the strategy only ever saw ours.
#[derive(Debug, Default)]
pub(in crate::live) struct OrderRegistry {
    next_id: u64,
    local_to_venue: SymMap<OrderId, String>,
    venue_to_local: SymMap<String, OrderId>,
    kind: SymMap<String, OrderKind>,
}

impl OrderRegistry {
    /// Mint the next unique local [`OrderId`].
    pub(in crate::live) fn mint(&mut self) -> OrderId {
        let id = OrderId(self.next_id);
        self.next_id += 1;
        id
    }

    /// Record a placed order's venue id + kind against a local id.
    pub(in crate::live) fn record(&mut self, local: OrderId, venue_id: &str, kind: OrderKind) {
        self.local_to_venue.insert(local, venue_id.to_string());
        self.venue_to_local.insert(venue_id.to_string(), local);
        self.kind.insert(venue_id.to_string(), kind);
    }

    pub(in crate::live) fn local_for(&self, venue_id: &str) -> Option<OrderId> {
        self.venue_to_local.get(venue_id).copied()
    }

    /// The kind a venue order was placed as. A fill on an order we didn't place
    /// (submitted out of band, through the venue's own UI) reads as `Market` —
    /// it moved the position, and that is all the strategy needs to know.
    pub(in crate::live) fn kind_for(&self, venue_id: &str) -> OrderKind {
        self.kind
            .get(venue_id)
            .copied()
            .unwrap_or(OrderKind::Market)
    }

    pub(in crate::live) fn venue_for(&self, local: OrderId) -> Option<&str> {
        self.local_to_venue.get(&local).map(String::as_str)
    }
}

/// A resting order we placed: a limit entry, or one protective leg.
///
/// Unifies OKX's `RestingLeg` + `RestingLimit` and Coinbase's `RestingOrder`.
/// `size` is **venue-native**; `price` is the trigger for a protective leg and
/// the limit for an entry — the same field under both of OKX's two names.
///
/// Kept so an unchanged re-submit is a no-op and a changed one cancels the
/// previous venue order before placing the replacement. The size is part of
/// that dedup key, so re-resting the same trigger for a *different* share
/// replaces the order rather than being mistaken for a no-op.
#[derive(Debug, Clone)]
pub(in crate::live) struct RestingOrder {
    pub(in crate::live) price: Real,
    pub(in crate::live) size: Real,
    pub(in crate::live) side: Side,
    pub(in crate::live) venue_id: String,
    pub(in crate::live) local: OrderId,
}

/// The resting protective bracket for one symbol: a stop leg and/or a
/// take-profit leg.
#[derive(Debug, Clone, Default)]
pub(in crate::live) struct Bracket {
    stop: Option<RestingOrder>,
    take_profit: Option<RestingOrder>,
}

impl Bracket {
    /// The leg `kind` selects. `Stop` is the default: `Market` / `Limit` never
    /// reach a bracket (a market exit is `set_position`, a resting entry is
    /// `set_limit`), and answering them as the stop leg is what every open-coded
    /// copy of this match already did.
    pub(in crate::live) fn leg(&self, kind: OrderKind) -> Option<&RestingOrder> {
        match kind {
            OrderKind::TakeProfit => self.take_profit.as_ref(),
            _ => self.stop.as_ref(),
        }
    }

    pub(in crate::live) fn set(&mut self, kind: OrderKind, leg: RestingOrder) {
        match kind {
            OrderKind::TakeProfit => self.take_profit = Some(leg),
            _ => self.stop = Some(leg),
        }
    }

    /// Whether either leg carries this local id.
    pub(in crate::live) fn leg_local(&self, local: OrderId) -> bool {
        [&self.stop, &self.take_profit]
            .into_iter()
            .any(|slot| slot.as_ref().is_some_and(|leg| leg.local == local))
    }

    /// Drop the leg with this local id; `true` if one matched.
    pub(in crate::live) fn clear_local(&mut self, local: OrderId) -> bool {
        for slot in [&mut self.stop, &mut self.take_profit] {
            if slot.as_ref().is_some_and(|leg| leg.local == local) {
                *slot = None;
                return true;
            }
        }
        false
    }

    /// Both legs, consuming — what `cancel_protective` walks.
    pub(in crate::live) fn into_legs(self) -> impl Iterator<Item = RestingOrder> {
        [self.stop, self.take_profit].into_iter().flatten()
    }
}

/// A live wallet's error log and its buffer of refused orders.
///
/// Both were unbounded `Vec`s on both backends, and `errors()` is a
/// *non-draining* accessor — so a months-long live run, the deployment these
/// wallets exist for, never freed either. [`PaperWallet`](crate::PaperWallet)
/// bounds its own two logs at [`DEFAULT_RETENTION`] for exactly that reason,
/// and this is the same bound reached through the same amortized
/// [`trim_front`].
///
/// The rejection drain is deliberately **not** the one `PaperWallet` uses.
/// There, `take_rejections` advances a cursor over a history that
/// `PaperWallet::rejections()` also exposes; here there is no history accessor
/// and nothing to preserve, so the drain takes the buffer outright. Sharing the
/// bound and the trim arithmetic is worth it; sharing the drain would mean
/// either growing a live `rejections()` accessor nobody asked for or deleting a
/// bound Python method.
#[derive(Debug)]
pub(in crate::live) struct LiveLog {
    errors: Vec<LiveError>,
    rejections: Vec<Rejection<Symbol>>,
    retention: Option<usize>,
}

impl Default for LiveLog {
    fn default() -> Self {
        Self {
            errors: Vec::new(),
            rejections: Vec::new(),
            retention: Some(DEFAULT_RETENTION),
        }
    }
}

impl LiveLog {
    /// Record a failure that has no return channel — a best-effort account
    /// refresh or fill poll, where the caller carries on with stale state.
    pub(in crate::live) fn note(&mut self, err: LiveError) {
        self.errors.push(err);
        self.trim();
    }

    /// Record `err` and return the trait-facing [`WalletError::Venue`]
    /// category, which is all a `Copy` enum has room to say.
    pub(in crate::live) fn fail(&mut self, err: LiveError) -> WalletError {
        self.note(err);
        WalletError::Venue
    }

    /// A **refused order**: log the detail, buffer a [`Rejection`] for
    /// [`take_rejections`](crate::Wallet::take_rejections) so the driver can
    /// route it to [`Strategy::on_reject`](crate::Strategy::on_reject), and
    /// return [`WalletError::Venue`].
    ///
    /// Unlike [`fail`](Self::fail), this is for a submission the strategy
    /// expected to place — an entry the venue rejects leaves the strategy flat
    /// when it wanted a position, a rejected protective leg leaves it holding
    /// one it wanted out of.
    pub(in crate::live) fn refuse(
        &mut self,
        symbol: &str,
        id: OrderId,
        kind: OrderKind,
        err: LiveError,
    ) -> WalletError {
        self.note(err);
        self.reject(symbol, id, WalletError::Venue, kind);
        WalletError::Venue
    }

    /// Buffer a refusal the wallet itself decided, with no venue error behind
    /// it — the short remainder a spot account cannot hold, say.
    pub(in crate::live) fn reject(
        &mut self,
        symbol: &str,
        id: OrderId,
        error: WalletError,
        kind: OrderKind,
    ) {
        self.rejections.push(Rejection {
            symbol: crate::types::symbol(symbol),
            id,
            error,
            kind,
        });
        self.trim();
    }

    /// The errors recorded so far, oldest first, bounded by the retention.
    pub(in crate::live) fn errors(&self) -> &[LiveError] {
        &self.errors
    }

    /// Hand over every buffered refusal and clear the buffer.
    pub(in crate::live) fn take_rejections(&mut self) -> Vec<Rejection<Symbol>> {
        std::mem::take(&mut self.rejections)
    }

    fn trim(&mut self) {
        trim_front(&mut self.errors, self.retention);
        trim_front(&mut self.rejections, self.retention);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(i: usize) -> LiveError {
        LiveError::Decode(i.to_string())
    }

    /// Churning past the bound must drop the *oldest*, not stop recording.
    ///
    /// A live wallet's error log is written from `update()`, so a venue that is
    /// down for a week appends on every bar with nothing draining it. Before
    /// the bound this grew for the process's whole life.
    #[test]
    fn the_error_log_is_bounded_and_keeps_the_newest() {
        let mut log = LiveLog::default();
        for i in 0..DEFAULT_RETENTION * 3 {
            log.note(decode(i));
        }
        assert!(
            log.errors().len() <= DEFAULT_RETENTION * 2,
            "error log grew past the trim threshold: {}",
            log.errors().len()
        );
        let newest = log.errors().last().expect("a retained error");
        assert_eq!(
            newest.to_string(),
            decode(DEFAULT_RETENTION * 3 - 1).to_string(),
            "trimming must drop the oldest, so the newest error always survives",
        );
    }

    /// The refusal buffer is bounded too, for the case that never drains it:
    /// a caller driving the wallet loop by hand who never calls
    /// `take_rejections`.
    #[test]
    fn the_rejection_buffer_is_bounded() {
        let mut log = LiveLog::default();
        for i in 0..DEFAULT_RETENTION * 3 {
            log.reject(
                "BTC-USD",
                OrderId(i as u64),
                WalletError::Venue,
                OrderKind::Market,
            );
        }
        assert!(
            log.take_rejections().len() <= DEFAULT_RETENTION * 2,
            "rejection buffer grew past the trim threshold",
        );
    }

    /// A refusal writes both logs at once, and the drain empties only the
    /// rejection side — the error detail stays readable through `errors()`
    /// after the driver has taken the rejection.
    #[test]
    fn a_refusal_records_the_detail_and_the_rejection_separately() {
        let mut log = LiveLog::default();
        let err = log.refuse("BTC-USD", OrderId(7), OrderKind::Stop, decode(1));
        assert_eq!(err, WalletError::Venue);

        let drained = log.take_rejections();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].id, OrderId(7));
        assert_eq!(drained[0].kind, OrderKind::Stop);

        assert_eq!(log.errors().len(), 1, "the detail outlives the drain");
        assert!(
            log.take_rejections().is_empty(),
            "a second drain must yield nothing"
        );
    }
}

/// Everything both live wallets keep that isn't credentials, signing, or the
/// venue's own account shape.
///
/// A Rust trait can't have fields, so the shared `Wallet` bodies in
/// [`flow`](super::flow) reach this through
/// [`VenueBackend`](super::VenueBackend)'s accessor pair. What stays outside is
/// exactly what differs in *kind* between venues: the credentials, the signing,
/// and how the account reports balances and positions.
#[derive(Debug)]
pub(in crate::live) struct LiveCore {
    pub(in crate::live) http: HttpCore,
    marks: SymMap<Symbol, Real>,
    grids: SymMap<Symbol, InstrumentGrid>,
    orders: OrderRegistry,
    protective: SymMap<Symbol, Bracket>,
    limits: SymMap<Symbol, RestingOrder>,
    cursors: SymMap<Symbol, FillCursor>,
    log: LiveLog,
}

impl LiveCore {
    pub(in crate::live) fn new(base_url: impl Into<String>) -> Self {
        Self {
            http: HttpCore::new(base_url),
            marks: SymMap::default(),
            grids: SymMap::default(),
            orders: OrderRegistry::default(),
            protective: SymMap::default(),
            limits: SymMap::default(),
            cursors: SymMap::default(),
            log: LiveLog::default(),
        }
    }

    // --- marks -------------------------------------------------------------

    pub(in crate::live) fn mark(&self, symbol: &str) -> Option<Real> {
        self.marks.get(symbol).copied()
    }

    pub(in crate::live) fn set_mark(&mut self, symbol: &Symbol, price: Real) {
        self.marks.insert(symbol.clone(), price);
    }

    /// Every symbol a candle has been fed for. On a venue whose account reports
    /// balances by *currency* rather than by product, this is the only way to
    /// name the products it holds.
    pub(in crate::live) fn marked_symbols(&self) -> impl Iterator<Item = &Symbol> {
        self.marks.keys()
    }

    /// Every fed mark. `ExactSizeIterator` because
    /// [`marked_sum`](crate::wallet::marked_sum) sizes its stack buffer from
    /// the count before it walks the values.
    pub(in crate::live) fn marks(&self) -> impl ExactSizeIterator<Item = (&Symbol, Real)> {
        self.marks.iter().map(|(s, &p)| (s, p))
    }

    // --- instrument grids --------------------------------------------------

    pub(in crate::live) fn grid(&self, symbol: &str) -> Option<InstrumentGrid> {
        self.grids.get(symbol).copied()
    }

    pub(in crate::live) fn cache_grid(&mut self, symbol: &str, grid: InstrumentGrid) {
        self.grids.insert(crate::types::symbol(symbol), grid);
    }

    // --- order ids ---------------------------------------------------------

    pub(in crate::live) fn orders(&self) -> &OrderRegistry {
        &self.orders
    }

    pub(in crate::live) fn orders_mut(&mut self) -> &mut OrderRegistry {
        &mut self.orders
    }

    pub(in crate::live) fn mint(&mut self) -> OrderId {
        self.orders.mint()
    }

    // --- resting orders ----------------------------------------------------

    pub(in crate::live) fn bracket(&self, symbol: &str) -> Option<&Bracket> {
        self.protective.get(symbol)
    }

    pub(in crate::live) fn set_leg(&mut self, symbol: &Symbol, kind: OrderKind, leg: RestingOrder) {
        self.protective
            .entry(symbol.clone())
            .or_default()
            .set(kind, leg);
    }

    pub(in crate::live) fn take_bracket(&mut self, symbol: &str) -> Option<Bracket> {
        self.protective.remove(symbol)
    }

    pub(in crate::live) fn clear_leg(&mut self, symbol: &str, local: OrderId) {
        if let Some(bracket) = self.protective.get_mut(symbol) {
            bracket.clear_local(local);
        }
    }

    pub(in crate::live) fn limit(&self, symbol: &str) -> Option<&RestingOrder> {
        self.limits.get(symbol)
    }

    pub(in crate::live) fn set_limit(&mut self, symbol: &Symbol, order: RestingOrder) {
        self.limits.insert(symbol.clone(), order);
    }

    pub(in crate::live) fn take_limit(&mut self, symbol: &str) -> Option<RestingOrder> {
        self.limits.remove(symbol)
    }

    /// The symbol whose resting limit carries this local id.
    pub(in crate::live) fn symbol_of_limit(&self, local: OrderId) -> Option<Symbol> {
        self.limits
            .iter()
            .find_map(|(sym, order)| (order.local == local).then(|| sym.clone()))
    }

    /// The symbol whose protective bracket carries this local id.
    pub(in crate::live) fn symbol_of_leg(&self, local: OrderId) -> Option<Symbol> {
        self.protective
            .iter()
            .find_map(|(sym, bracket)| bracket.leg_local(local).then(|| sym.clone()))
    }

    // --- fill cursors ------------------------------------------------------

    pub(in crate::live) fn has_cursor(&self, symbol: &str) -> bool {
        self.cursors.contains_key(symbol)
    }

    pub(in crate::live) fn seed_cursor(&mut self, symbol: &str, cursor: FillCursor) {
        self.cursors.insert(crate::types::symbol(symbol), cursor);
    }

    pub(in crate::live) fn take_cursor(&mut self, symbol: &str) -> Option<FillCursor> {
        self.cursors.remove(symbol)
    }

    /// Every symbol with a cursor — the ones this wallet has traded, and so the
    /// ones [`poll_fills`](crate::Wallet::poll_fills) has to sweep.
    pub(in crate::live) fn cursor_symbols(&self) -> Vec<Symbol> {
        self.cursors.keys().cloned().collect()
    }

    // --- logs --------------------------------------------------------------

    pub(in crate::live) fn log(&self) -> &LiveLog {
        &self.log
    }

    pub(in crate::live) fn log_mut(&mut self) -> &mut LiveLog {
        &mut self.log
    }
}
