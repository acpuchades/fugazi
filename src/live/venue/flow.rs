//! The shared [`Wallet`] bodies: the order flow every live venue runs, spelled
//! once.
//!
//! These are **free generic functions**, not provided methods on
//! [`VenueBackend`]. A provided `update` would collide with the `Wallet::update`
//! it implements — `fn update(&mut self, ..) { self.update(..) }` is either
//! infinite recursion or an inference error — so every shared body would need a
//! second name, at which point "this is the shared body of `Wallet::update`"
//! becomes a naming convention rather than a fact. A free function also cannot
//! be silently overridden: a venue that needs different behaviour has to add a
//! hook, which is a visible diff on the trait.
//!
//! **Borrow discipline.** A shared body interleaves core mutation with hook
//! calls, and you cannot hold a `&mut` into the core across a hook call. Every
//! body here is therefore a straight-line sequence of statements that re-borrow
//! through [`VenueBackend::core_mut`]; if one starts wanting to bind a `&mut`
//! above a hook call, split the statement rather than restructuring the trait.

use crate::types::{Candle, Real, Symbol};
use crate::wallet::{
    Ack, Order, OrderId, OrderKind, POSITION_EPSILON, PRICE_EPSILON, Reference, Side, Size, Units,
    WalletError,
};

use super::super::LiveError;
use super::{FillCursor, InstrumentGrid, OrderClass, RestingOrder, VenueBackend, VenueFill};

// --- logging shorthands -----------------------------------------------------

fn note<V: VenueBackend>(v: &mut V, err: LiveError) {
    v.core_mut().log_mut().note(err);
}

fn fail<V: VenueBackend>(v: &mut V, err: LiveError) -> WalletError {
    v.core_mut().log_mut().fail(err)
}

fn refuse<V: VenueBackend>(
    v: &mut V,
    symbol: &str,
    id: OrderId,
    kind: OrderKind,
    err: LiveError,
) -> WalletError {
    v.core_mut().log_mut().refuse(symbol, id, kind, err)
}

// --- caches -----------------------------------------------------------------

/// The cached [`InstrumentGrid`] for `symbol`, fetching it on first use.
///
/// Never invalidated: a venue that re-tiers an instrument mid-run keeps the
/// grid it had at first touch, which is the same bargain a once-per-symbol
/// fetch always makes.
pub(in crate::live) fn ensure_grid<V: VenueBackend>(
    v: &mut V,
    symbol: &str,
) -> Result<InstrumentGrid, LiveError> {
    if let Some(grid) = v.core().grid(symbol) {
        return Ok(grid);
    }
    let grid = v.fetch_grid(symbol)?;
    v.core_mut().cache_grid(symbol, grid);
    Ok(grid)
}

/// Ensure a fill cursor exists for `symbol`, seeded past the fills already on
/// the venue — so we only ever report fills that happen *after* we started
/// trading it, not the account's whole history.
///
/// Every submission path calls this **before** placing, so an order that fills
/// immediately is caught by the next poll rather than skipped by a cursor
/// seeded past its own fill.
pub(in crate::live) fn ensure_cursor<V: VenueBackend>(
    v: &mut V,
    symbol: &str,
) -> Result<(), LiveError> {
    if v.core().has_cursor(symbol) {
        return Ok(());
    }
    let history = v.fetch_fills(symbol)?;
    let cursor = FillCursor::seeded(v.cursor_model(), &history);
    v.core_mut().seed_cursor(symbol, cursor);
    Ok(())
}

// --- the bar loop -----------------------------------------------------------

/// Feed a bar: record the mark, refresh the account, and report whatever the
/// venue has filled since the last call.
///
/// The account refresh is **best effort** — `update` returns fills, not a
/// `Result`, so a venue that has gone away degrades to "no fills this bar, and
/// the reads answer from the last good state" rather than aborting the run.
pub(in crate::live) fn update<V: VenueBackend>(
    v: &mut V,
    symbol: Symbol,
    candle: Candle,
) -> Vec<Order<Symbol>> {
    v.core_mut().set_mark(&symbol, candle.close);
    if let Err(e) = v.refresh() {
        note(v, e);
    }
    // Seeding here as well as at submission covers the symbol whose first
    // contact with the venue is a bar rather than an order.
    if let Err(e) = ensure_cursor(v, &symbol) {
        note(v, e);
        return Vec::new();
    }
    match poll_symbol(v, &symbol) {
        Ok(fills) => fills,
        Err(e) => {
            note(v, e);
            Vec::new()
        }
    }
}

/// Poll every symbol this wallet has traded for fills it hasn't reported yet.
pub(in crate::live) fn poll_fills<V: VenueBackend>(v: &mut V) -> Vec<Order<Symbol>> {
    let mut out = Vec::new();
    for symbol in v.core().cursor_symbols() {
        match poll_symbol(v, &symbol) {
            Ok(mut fills) => out.append(&mut fills),
            Err(e) => note(v, e),
        }
    }
    out
}

/// New fills for `symbol`, oldest first, converted to base units.
///
/// A venue order we placed maps back to its local [`OrderId`] and recorded
/// [`OrderKind`]; a fill on an order we don't know — submitted out of band,
/// through the venue's own UI — gets a fresh local id and `Market` kind, since
/// it moved the position and that is all the strategy needs to know.
pub(in crate::live) fn poll_symbol<V: VenueBackend>(
    v: &mut V,
    symbol: &Symbol,
) -> Result<Vec<Order<Symbol>>, LiveError> {
    let grid = ensure_grid(v, symbol)?;
    let mut fills = v.fetch_fills(symbol)?;
    fills.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));

    let mut cursor = match v.core_mut().take_cursor(symbol) {
        Some(c) => c,
        None => FillCursor::seeded(v.cursor_model(), &[]),
    };
    let fresh: Vec<VenueFill> = fills.into_iter().filter(|f| cursor.admit(f)).collect();
    v.core_mut().seed_cursor(symbol, cursor);

    let mut out = Vec::with_capacity(fresh.len());
    for f in fresh {
        let local = match v.core().orders().local_for(&f.order_id) {
            Some(id) => id,
            None => v.core_mut().mint(),
        };
        let kind = v.core().orders().kind_for(&f.order_id);
        out.push(
            Order::new(
                symbol.clone(),
                f.side,
                grid.base_units(f.size),
                f.price,
                kind,
                local,
            )
            .with_commission(f.commission),
        );
    }
    Ok(out)
}

// --- movement ---------------------------------------------------------------

/// Drive `symbol` to a signed target in base units with a market order.
///
/// **The `can_short` clamp here is the impl enforcing its own limit, not the
/// trait enforcing it.** `Wallet::can_short` informs a caller; a venue that
/// cannot hold a short still has to do something sensible with a negative
/// target, and selling to flat while booking an `UnsupportedOperation` for the
/// remainder is that something — the strategy learns its short didn't take
/// rather than silently believing it did. Doing it in shared code doesn't
/// change whose responsibility it is.
///
/// A diff that rounds below the venue's minimum **accepts the submission and
/// places nothing**: no fill will arrive under the returned id. That is the
/// right answer for an entry (a strategy inching towards a target would
/// otherwise get a rejection per bar) and the wrong one for a protective leg,
/// which refuses instead — see [`rest_protective`].
pub(in crate::live) fn set_position<V: VenueBackend>(
    v: &mut V,
    target: Units<Symbol>,
) -> Result<Ack<Symbol>, WalletError> {
    let symbol = target.symbol;
    // Minted up front so a refusal before the POST still carries the
    // submission's id into its `Rejection`.
    let id = v.core_mut().mint();
    let grid = match ensure_grid(v, &symbol) {
        Ok(g) => g,
        Err(e) => return Err(refuse(v, &symbol, id, OrderKind::Market, e)),
    };

    let current = v.position(&symbol).amount;
    let mut wanted = target.amount;
    if !v.can_short() && wanted < -POSITION_EPSILON {
        v.core_mut().log_mut().reject(
            &symbol,
            id,
            WalletError::UnsupportedOperation,
            OrderKind::Market,
        );
        wanted = 0.0;
    }
    let delta = wanted - current;
    let size = grid.venue_size(delta.abs());
    if grid.below_minimum(size) {
        return Ok(Ack::Working(id));
    }
    if let Err(e) = ensure_cursor(v, &symbol) {
        note(v, e);
    }
    let side = if delta > 0.0 { Side::Buy } else { Side::Sell };
    let venue_id = match v.place_market(&symbol, side, size, &grid, id) {
        Ok(venue_id) => venue_id,
        Err(e) => return Err(refuse(v, &symbol, id, OrderKind::Market, e)),
    };
    v.core_mut()
        .orders_mut()
        .record(id, &venue_id, OrderKind::Market);
    Ok(Ack::Working(id))
}

/// Rest a `limit` / GTC entry on the venue.
///
/// The [`Size`] resolves at the **limit price** — that is where the order
/// fills, so it is the price the target should be sized against, matching
/// [`PaperWallet`](crate::PaperWallet)'s "resolve at the fill" rule.
///
/// The venue order's side comes from the resolved *delta*, not from `side`
/// directly: `side · size` is an absolute target, so reducing a long is a sell
/// however the caller spelled it. A limit already through the market is simply
/// a marketable limit — the venue fills it at the limit or better.
///
/// Idempotent per symbol: re-submitting the same side / price / size is a no-op
/// returning the resting order's id; any other change cancels the previous
/// venue order before placing the replacement, so a strategy can walk its limit
/// every bar without piling up orders.
pub(in crate::live) fn set_limit<V: VenueBackend>(
    v: &mut V,
    symbol: Symbol,
    side: Side,
    size: Size,
    limit: Reference,
) -> Result<Ack<Symbol>, WalletError> {
    let local = v.core_mut().mint();
    if limit.0 <= 0.0 {
        let err = LiveError::Decode(format!("limit price must be positive, got {}", limit.0));
        return Err(refuse(v, &symbol, local, OrderKind::Limit, err));
    }
    let grid = match ensure_grid(v, &symbol) {
        Ok(g) => g,
        Err(e) => return Err(refuse(v, &symbol, local, OrderKind::Limit, e)),
    };

    let current = v.position(&symbol).amount;
    let units = size.resolve(limit.0, current, v.funds().0, v.equity().0);
    let mut delta = side.sign() * units - current;
    if !v.can_short() && delta < 0.0 {
        // An account that holds no short cannot sell more base than it has.
        delta = delta.max(-current);
    }
    let size = grid.venue_size(delta.abs());
    let price = grid.on_tick(limit.0);
    if grid.below_minimum(size) {
        return Ok(Ack::Working(local));
    }
    let order_side = if delta > 0.0 { Side::Buy } else { Side::Sell };

    if let Some(existing) = v.core().limit(&symbol).cloned() {
        if existing.side == order_side
            && (existing.price - price).abs() <= PRICE_EPSILON
            && (existing.size - size).abs() <= POSITION_EPSILON
        {
            return Ok(Ack::Working(existing.local));
        }
        cancel_resting(v, &symbol, &existing.venue_id, OrderClass::Entry)?;
        v.core_mut().take_limit(&symbol);
    }

    if let Err(e) = ensure_cursor(v, &symbol) {
        note(v, e);
    }
    let venue_id = match v.place_limit(&symbol, order_side, size, price, &grid, local) {
        Ok(venue_id) => venue_id,
        Err(e) => return Err(refuse(v, &symbol, local, OrderKind::Limit, e)),
    };
    v.core_mut()
        .orders_mut()
        .record(local, &venue_id, OrderKind::Limit);
    v.core_mut().set_limit(
        &symbol,
        RestingOrder {
            price,
            size,
            side: order_side,
            venue_id,
            local,
        },
    );
    Ok(Ack::Working(local))
}

/// Withdraw the resting limit on `symbol`, if any.
pub(in crate::live) fn cancel_limit<V: VenueBackend>(
    v: &mut V,
    symbol: &Symbol,
) -> Result<(), WalletError> {
    if let Some(resting) = v.core_mut().take_limit(symbol) {
        cancel_resting(v, symbol, &resting.venue_id, OrderClass::Entry)?;
    }
    Ok(())
}

/// Rest a protective leg with idempotent dedup, reduce-only.
///
/// Three guards, and each one refuses rather than silently accepting: a
/// non-positive trigger is nonsense on any venue; a size that rounds to nothing
/// or below the venue's minimum would be rejected on arrival. The asymmetry
/// with [`set_position`] / [`set_limit`], which accept-and-place-nothing below
/// the minimum, is deliberate — a silently-accepted protective leg leaves the
/// strategy believing it is protected when no order exists.
///
/// The share resolves against the **mark**, falling back to the trigger when no
/// bar has been fed yet. Falling back to zero instead would make every
/// fraction-sized stop resolve to nothing and refuse, which is exactly what a
/// strategy resting a stop before its first `update` would hit.
pub(in crate::live) fn rest_protective<V: VenueBackend>(
    v: &mut V,
    symbol: Symbol,
    kind: OrderKind,
    trigger: Real,
    size: Size,
) -> Result<Ack<Symbol>, WalletError> {
    let local = v.core_mut().mint();
    if trigger <= 0.0 {
        let err = LiveError::Decode(format!(
            "protective trigger must be positive, got {trigger}"
        ));
        return Err(refuse(v, &symbol, local, kind, err));
    }
    let grid = match ensure_grid(v, &symbol) {
        Ok(g) => g,
        Err(e) => return Err(refuse(v, &symbol, local, kind, e)),
    };

    // Reduce-only: resolve the share, then clamp it to what is actually held.
    let held = v.position(&symbol).amount;
    let at = v.core().mark(&symbol).unwrap_or(trigger);
    let units = size
        .resolve(at, held, v.funds().0, v.equity().0)
        .min(held.abs());
    let venue_size = grid.venue_size(units);
    if grid.below_minimum(venue_size) {
        let err = LiveError::Decode(
            "protective size rounds to nothing, or below the venue minimum".into(),
        );
        return Err(refuse(v, &symbol, local, kind, err));
    }
    let price = grid.on_tick(trigger);

    if let Some(leg) = v.core().bracket(&symbol).and_then(|b| b.leg(kind)).cloned() {
        if (leg.price - price).abs() <= PRICE_EPSILON
            && (leg.size - venue_size).abs() <= POSITION_EPSILON
        {
            return Ok(Ack::Working(leg.local));
        }
        cancel_resting(v, &symbol, &leg.venue_id, OrderClass::Protective)?;
    }

    // A protective exit trades the opposite side of the open position. `held`
    // is non-zero here: a flat account clamps `units` to zero above.
    let side = if held > 0.0 { Side::Sell } else { Side::Buy };
    if let Err(e) = ensure_cursor(v, &symbol) {
        note(v, e);
    }
    let venue_id = match v.place_protective(&symbol, kind, side, venue_size, price, &grid, local) {
        Ok(venue_id) => venue_id,
        Err(e) => return Err(refuse(v, &symbol, local, kind, e)),
    };
    v.core_mut().orders_mut().record(local, &venue_id, kind);
    v.core_mut().set_leg(
        &symbol,
        kind,
        RestingOrder {
            price,
            size: venue_size,
            side,
            venue_id,
            local,
        },
    );
    Ok(Ack::Working(local))
}

/// Withdraw both protective legs on `symbol`.
pub(in crate::live) fn cancel_protective<V: VenueBackend>(
    v: &mut V,
    symbol: &Symbol,
) -> Result<(), WalletError> {
    if let Some(bracket) = v.core_mut().take_bracket(symbol) {
        for leg in bracket.into_legs() {
            cancel_resting(v, symbol, &leg.venue_id, OrderClass::Protective)?;
        }
    }
    Ok(())
}

/// Withdraw one order by the local id its submission returned.
///
/// An id the wallet never issued, or one that belongs to a market order rather
/// than a resting record, is a no-op: the post-condition — that order is not
/// resting — already holds. A market order fills near-instantly and is not
/// tracked for cancel.
pub(in crate::live) fn cancel<V: VenueBackend>(v: &mut V, id: OrderId) -> Result<(), WalletError> {
    let Some(venue_id) = v.core().orders().venue_for(id).map(str::to_string) else {
        return Ok(());
    };
    if let Some(symbol) = v.core().symbol_of_limit(id) {
        cancel_resting(v, &symbol, &venue_id, OrderClass::Entry)?;
        v.core_mut().take_limit(&symbol);
        return Ok(());
    }
    let Some(symbol) = v.core().symbol_of_leg(id) else {
        return Ok(());
    };
    cancel_resting(v, &symbol, &venue_id, OrderClass::Protective)?;
    v.core_mut().clear_leg(&symbol, id);
    Ok(())
}

/// Cancel a venue order, logging the detail behind a failure.
///
/// A cancel failure is a [`fail`], not a [`refuse`]: nothing was *submitted*,
/// so there is no order for the strategy's `on_reject` to hear about.
fn cancel_resting<V: VenueBackend>(
    v: &mut V,
    symbol: &str,
    venue_id: &str,
    class: OrderClass,
) -> Result<(), WalletError> {
    match v.cancel_venue_order(symbol, venue_id, class) {
        Ok(()) => Ok(()),
        Err(e) => Err(fail(v, e)),
    }
}
