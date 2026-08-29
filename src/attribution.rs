//! Per-child decomposition of a **composite** strategy's run.
//!
//! A [`Portfolio`](crate::portfolio::Portfolio) nets its children's intents into
//! one order per symbol before anything reaches the account, so its
//! [`RunReport::fills`](crate::RunReport::fills) is a stream of *account* fills
//! with no trace of which child asked for which unit. When two children trade
//! the same symbol — the ordinary case, since a portfolio exists to combine
//! views — that stream cannot answer the only question worth asking about a
//! composite under a regime change: **which child stopped contributing?**
//!
//! The decomposition is not a reconstruction. The portfolio already computes it
//! on every bar, because it has to: each child holds a notional
//! [`Ledger`](crate::portfolio) that must be moved by that child's own share of
//! every fill, and each child's `on_fill` must see its own share and no one
//! else's. This module is the retained record of that split, so a caller gets
//! the engine's own answer rather than an estimate.
//!
//! # Why the parts do not add up to the whole
//!
//! The tempting workaround — run each child standalone over the same bars and
//! difference the equity curves — is *wrong*, not merely imprecise. Children on
//! one account share cash, so a sibling's entry can starve another's; netting
//! removes flow that would otherwise have crossed the spread twice; and a
//! rebalance moves capital between them on its own schedule. None of that is
//! visible to a standalone run, and the error does not distribute evenly: it
//! lands on whichever child the netting actually touched.
//!
//! # What is attributed, and how
//!
//! [`Attribution::fills`] carries one [`ChildFill`] per child per booked
//! movement, each a full [`Order`] with that child's own units, *its own
//! effective price* (blended across the crossed and market parts of its flow)
//! and its pro-rata share of the commission. Two properties hold by
//! construction and are asserted in the suite:
//!
//! - **Signed units sum to the account.** Summing `side.sign() * units` over
//!   every `ChildFill` gives exactly what the same sum over
//!   [`RunReport::fills`](crate::RunReport::fills) gives.
//! - **[`Attribution::equity`] sums to the account curve, bar by bar.** Not
//!   just at the end — every row sums to that bar's
//!   [`RunReport::equity_curve`](crate::RunReport::equity_curve) entry, for
//!   every bar of every run. This is the portfolio's core
//!   `Σ ledgers == account` invariant, sampled per bar.
//!
//!   The one thing that is *not* a residual to render: from a
//!   [`ruin_bar`](crate::RunReport::ruin_bar) onward both sides read `0.0`,
//!   because the account curve is pinned there and these rows are pinned with
//!   it. The children did not simultaneously go to zero — the account did, and
//!   nothing past it is recorded on either side. See
//!   [`RunReport::ruin_bar`](crate::RunReport::ruin_bar).
//!
//! # A `ChildFill` is not a breakdown of an account fill
//!
//! There are `ChildFill`s with **no corresponding account fill at all**. When
//! children's intents cross internally — one buying what another is selling —
//! the net is zero, so nothing is submitted and no fill ever arrives; the
//! portfolio books that flow against the ledgers at the next bar's open
//! instead. Those entries carry [`ChildFill::crossed`] set.
//!
//! This is why the attribution is a *stream* rather than a `[(child, units)]`
//! field hung off [`Order`]: a breakdown keyed to account fills would silently
//! drop exactly the flow a portfolio exists to create. It is also why
//! `fills.len()` is generally neither the account's fill count nor a multiple
//! of it.

use crate::types::Real;
use crate::wallet::Order;

/// One child's share of a single booked movement.
///
/// A full [`Order`] rather than a bare unit count, because a child's own
/// execution price differs from the account's whenever part of its flow crossed
/// internally — and a per-child P&L computed at the account's price would not
/// reconcile against that child's ledger.
#[derive(Debug, Clone)]
pub struct ChildFill<Sym> {
    /// Zero-based index of the child, in `add` order — the same index
    /// [`Portfolio::sub_equity`](crate::portfolio::Portfolio::sub_equity) and
    /// [`Attribution::names`] use. Index is the identity throughout a
    /// portfolio; the name is a label carried alongside.
    pub child: usize,
    /// Zero-based index into the input snapshot stream, matching
    /// [`Fill::bar`](crate::backtest::Fill::bar).
    pub bar: usize,
    /// This child's share: its own signed units at its own effective price,
    /// carrying its pro-rata slice of the account fill's commission.
    pub order: Order<Sym>,
    /// Whether this movement was booked against a sibling rather than the
    /// market — flow that netted to zero, so **no account fill exists for it**.
    ///
    /// Worth reading rather than ignoring: crossed flow costs no spread and no
    /// commission, so a child whose exits are routinely absorbed by a sibling's
    /// entries is being subsidised by the composite in a way a standalone run
    /// of it would never show.
    pub crossed: bool,
}

/// The per-child decomposition of one run — see the [module
/// docs](self) for what is guaranteed to reconcile.
///
/// Reached through [`RunReport::attribution`](crate::RunReport::attribution),
/// which is `Some` for a portfolio and `None` for every non-composite shape.
#[derive(Debug, Clone, Default)]
pub struct Attribution<Sym> {
    names: Vec<String>,
    fills: Vec<ChildFill<Sym>>,
    equity: Vec<Vec<Real>>,
}

impl<Sym> Attribution<Sym> {
    /// Assemble from a portfolio's retained buffers. Crate-internal: the
    /// invariants documented on the accessors hold only for buffers the
    /// netting layer produced.
    pub(crate) fn new(
        names: Vec<String>,
        fills: Vec<ChildFill<Sym>>,
        equity: Vec<Vec<Real>>,
    ) -> Self {
        Self {
            names,
            fills,
            equity,
        }
    }

    /// Pin every row from `bar` onward to zero, mirroring the driver's own
    /// post-[ruin](crate::RunReport::ruin_bar) pin on
    /// [`equity_curve`](crate::RunReport::equity_curve).
    ///
    /// Not cosmetic, and not only for the sum's sake. A ruined account's marks
    /// keep moving — a short's loss is unbounded above, so the ledgers behind
    /// these rows go on tracking it — and a per-child curve allowed below zero
    /// reports further losses as *gains*, since `(e - prev) / prev` inverts sign
    /// once `prev < 0`. That is the whole reason the account curve is pinned;
    /// [`child_equity`](Self::child_equity) is a curve people take returns off
    /// in exactly the same way, so it is pinned in exactly the same place.
    pub(crate) fn pin_from(&mut self, bar: usize) {
        for row in self.equity.iter_mut().skip(bar) {
            row.iter_mut().for_each(|e| *e = 0.0);
        }
    }

    /// The children's names, in `add` order — index `i` names child `i`.
    pub fn names(&self) -> &[String] {
        &self.names
    }

    /// How many children the composite ran.
    pub fn child_count(&self) -> usize {
        self.names.len()
    }

    /// Every per-child movement, in the order the portfolio booked them.
    ///
    /// Group by [`ChildFill::child`] to get one child's blotter, which
    /// [`reconstruct_trades`](crate::metrics::reconstruct_trades) will reduce to
    /// round trips like any other.
    pub fn fills(&self) -> &[ChildFill<Sym>] {
        &self.fills
    }

    /// Per-bar per-child mark-to-market equity: `equity()[bar][child]`.
    ///
    /// One row per bar the composite was advanced over, each summing to that
    /// bar's [`RunReport::equity_curve`](crate::RunReport::equity_curve) entry.
    /// This is what attributes **risk** rather than realized P&L — a child's
    /// share of variance comes from its equity path, not from its fills, and a
    /// child that quietly stopped taking risk shows up here and nowhere else.
    pub fn equity(&self) -> &[Vec<Real>] {
        &self.equity
    }

    /// Child `idx`'s equity across the run — the `idx` column of
    /// [`equity`](Self::equity).
    ///
    /// # Panics
    /// Panics if `idx` is out of range.
    pub fn child_equity(&self, idx: usize) -> Vec<Real> {
        assert!(
            idx < self.names.len(),
            "child index {idx} out of range (portfolio has {} children)",
            self.names.len()
        );
        self.equity.iter().map(|row| row[idx]).collect()
    }
}
