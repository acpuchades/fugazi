//! Drive a [`Strategy`] over a bar series through a [`Wallet`], recording the
//! two artefacts every post-run analytic reduces to: the **equity curve** (one
//! mark-to-market point per bar) and the **fill blotter** (each booked order,
//! tagged with the bar it filled on).
//!
//! This is the pure primitive. It does no I/O, no formatting, and takes no
//! opinion on what to do with the report — a CLI backtester turns it into
//! `fills.csv` / `trades.csv` / `returns.csv` / `metrics.yml`, an optimizer
//! runs it once per parameter combination, a Python binding hands it to a
//! notebook. The
//! [`Wallet`] is generic (taken as `&mut impl Wallet<Sym>`) so the same
//! primitive drives a [`PaperWallet`](crate::PaperWallet) backtest or a live
//! broker-backed impl unchanged — it's not backtest-only, hence the neutral
//! [`run`] name.
//!
//! Bars enter as [`Snapshot<Sym>`](crate::types::Snapshot)s — a per-bar
//! keyed collection of tagged [`Atom`](crate::Atom)s. Each snapshot represents "all ticks
//! at time `t`" — an entry per symbol that traded at that time, tagged with
//! its symbol and (optionally) frequency. Per bar, in order: walk every
//! `(symbol, atom)` entry the snapshot carries and feed the wallet
//! `wallet.update(symbol, atom.candle)` — every symbol the wallet holds a
//! position in gets marked to market on the same bar; the fill stream those
//! updates return is routed to [`Strategy::on_fill`] and collected into the
//! blotter. Then [`Strategy::update`] the strategy with the whole snapshot,
//! and [`Strategy::trade`] it (queuing this bar's market orders —
//! [`PaperWallet`](crate::PaperWallet) fills them at the next bar's `open`).
//! The bar's mark-to-market equity is appended last.
//!
//! Untagged entries (`symbol = None`) are skipped for wallet pricing —
//! there's no symbol to price against. The strategy still sees them via
//! `snap`, so leaves that use the empty-selector [`Pick::new`](crate::indicators::Pick::new)
//! (the single-series sole-atom unpack) still work; but no fills are booked
//! for them. Callers that want the wallet priced need to tag their entries
//! (typically via [`Snapshot::single(sym, atom)`](crate::types::Snapshot::single)
//! for the single-series shortcut, or [`Snapshot::push`](crate::types::Snapshot::push)
//! for multi-asset).

use crate::attribution::Attribution;
use crate::types::Snapshot;
use crate::types::Symbol;
use crate::wallet::Rejection;
use crate::{Order, Real, Strategy, Wallet};

/// One booked order stamped with the bar index it filled on.
///
/// Held in [`RunReport::fills`] in fill order — the same order the wallet
/// booked them. `bar` is the zero-based position in the input snapshot stream
/// at which the fill occurred (which, for [`PaperWallet`](crate::PaperWallet),
/// is the bar whose `open` the fill traded at, i.e. one bar after the signal).
#[derive(Debug, Clone)]
pub struct Fill<Sym> {
    /// Zero-based index into the input snapshot stream.
    pub bar: usize,
    /// The order that filled, as booked by the wallet (side, units, price, kind,
    /// id — see [`Order`]).
    pub order: Order<Sym>,
}

/// One refused order stamped with the bar index it was refused on.
///
/// The failure-side twin of [`Fill`]. Held in [`RunReport::rejections`] in the
/// order the wallet booked them.
#[derive(Debug, Clone)]
pub struct Rejected<Sym> {
    /// Zero-based index into the input snapshot stream.
    pub bar: usize,
    /// The order that was refused, and why (see [`Rejection`]).
    pub rejection: Rejection<Sym>,
}

/// Everything a post-run analytic needs to reduce one run to numbers.
///
/// - [`equity_curve`](Self::equity_curve) holds one mark-to-market equity value
///   per input snapshot, in bar order.
/// - [`fills`](Self::fills) holds every order the wallet booked over the run,
///   in fill order, each tagged with the bar index it filled on.
/// - [`initial_equity`](Self::initial_equity) is the wallet's total equity
///   captured **before the first bar** — the seed value returns / CAGR compound
///   against.
#[derive(Debug, Clone)]
pub struct RunReport<Sym> {
    /// One entry per input snapshot, in bar order (post mark-to-market).
    pub equity_curve: Vec<Real>,
    /// Every booked fill, in the order the wallet produced them.
    pub fills: Vec<Fill<Sym>>,
    /// Every order the wallet **refused**, in the order it refused them.
    ///
    /// Empty on a clean run, and empty for any [`Wallet`] that does not override
    /// [`take_rejections`](Wallet::take_rejections). A non-empty list means the
    /// run's equity curve reflects trades that did not happen the way the
    /// strategy intended — check it before trusting the metrics.
    pub rejections: Vec<Rejected<Sym>>,
    /// Total wallet equity captured immediately before the first bar.
    pub initial_equity: Real,
    /// The bar the account was **ruined** on — the first bar close at which
    /// total equity was `<= 0` — or `None` for a run that stayed solvent.
    ///
    /// Ruin is a terminal run outcome, not a metrics curiosity. On that bar
    /// [`run`] liquidates the book through [`Wallet::flatten`], submits nothing
    /// further, and pins every remaining
    /// [`equity_curve`](Self::equity_curve) entry — the ruin bar's included —
    /// at `0.0`. So a ruined run reports exactly `-100%` total return, a max
    /// drawdown of exactly 100%, and no fill after this index.
    ///
    /// Without this the simulation traded on through negative equity, and
    /// `(e - prev) / prev` turns *further losses* into **positive** returns
    /// once `prev < 0` — a region of parameter space with a genuinely positive
    /// Sharpe that any argmax search finds.
    pub ruin_bar: Option<usize>,
    /// `(bars that wanted a carry rate, bars that got one)`, or `None` when the
    /// account does not model carry — see [`Wallet::carry_coverage`].
    ///
    /// [`Wallet::carry_coverage`]: crate::Wallet::carry_coverage
    ///
    /// Sits beside [`rejections`](Self::rejections) for the same reason: both
    /// say the equity curve above them may not describe what it looks like it
    /// describes. `seen < wanted` means a data-driven carry model was configured
    /// and charged **nothing** on the difference, which is indistinguishable
    /// from carry being free — the exact failure that leg exists to remove.
    pub carry_coverage: Option<(usize, usize)>,
    /// The run's **per-child decomposition**, for a composite strategy that
    /// has one — `Some` for a [`Portfolio`](crate::portfolio::Portfolio),
    /// `None` for every other shape.
    ///
    /// A portfolio nets its children's intents into one order per symbol before
    /// anything reaches the account, so [`fills`](Self::fills) above is a stream
    /// of *account* fills that cannot be split after the fact — which child
    /// asked for a given unit is not recoverable from it. This carries the split
    /// the portfolio already made in order to move each child's ledger, so
    /// "which of my children stopped contributing?" is answerable from the
    /// report rather than by re-running each child standalone (which does not
    /// reproduce the composite — see [`Attribution`]).
    pub attribution: Option<Attribution<Sym>>,
}

impl<Sym> RunReport<Sym> {
    /// How many fills traded **materially** less than the strategy asked for,
    /// and the worst ratio among them — or `None` on a run where every fill got
    /// what it wanted.
    ///
    /// The third member of the "the equity curve above may not describe what it
    /// looks like it describes" family, beside
    /// [`rejections`](Self::rejections) and
    /// [`carry_coverage`](Self::carry_coverage), and the one that was missing.
    /// A refusal lands in `rejections`; a *fitted* fill is not a refusal — the
    /// trade happened — so it landed nowhere, and a `sizing:` the account could
    /// not carry was indistinguishable in the blotter from a signal that simply
    /// sized smaller. Every metric downstream then described the size that
    /// traded rather than the size that was requested.
    ///
    /// The gap has always been on the fill as
    /// [`requested_units`](crate::wallet::Order::requested_units); what was
    /// missing was a way to find it without comparing two fields on every fill
    /// of every run. `Some((n, worst))` means *go look*: `n` fills were cut,
    /// the worst to `worst` of its request.
    ///
    /// Materiality is [`MATERIALLY_FITTED`](crate::wallet::MATERIALLY_FITTED),
    /// not "anything below `1.0`" — a costed all-in sheds a sliver every time,
    /// and reporting that would make the signal useless.
    pub fn materially_fitted(&self) -> Option<(usize, Real)> {
        let mut n = 0usize;
        let mut worst = Real::INFINITY;
        for fill in &self.fills {
            if fill.order.is_materially_fitted() {
                n += 1;
                worst = worst.min(fill.order.fill_ratio());
            }
        }
        (n > 0).then_some((n, worst))
    }
}

/// Drive `strategy` over `snapshots`, executing against `wallet`, and return
/// the [`RunReport`].
///
/// The reported [`equity_curve`](RunReport::equity_curve) has one entry per
/// bar (post mark-to-market for that bar). The reported
/// [`fills`](RunReport::fills) are the wallet's fill stream: for
/// [`PaperWallet`](crate::PaperWallet), the previous bar's queued market orders
/// filling at this bar's `open`, plus any resting protective legs this bar
/// triggered.
///
/// Per bar, `run` walks every `(symbol, atom)` entry in the snapshot and
/// feeds the wallet `wallet.update(symbol, atom.candle)` — so every symbol
/// the wallet holds a position in gets marked to market. Untagged entries
/// (`symbol = None`) are skipped for wallet pricing (nothing to price
/// against); the strategy still sees them in `snap`. The strategy carries
/// its own trading symbol on its `S::Symbol` state and uses it inside
/// `trade` / `on_fill` — [`run`] does not need to know which symbol is
/// "the strategy's own", so the same signature drives a single-asset
/// strategy over a single-entry snapshot and a multi-asset strategy over a
/// multi-entry one.
///
/// The wallet is passed in so the caller controls initial cash, wallet
/// implementation (paper vs. downstream broker-backed), and any pre-warming.
/// `snapshots` is any iterable over anything convertible to
/// [`Snapshot<S::Symbol>`] — pass `Vec<Snapshot<Sym>>` directly, or a
/// `Vec<Atom>` / `Vec<Candle>` for a single-series run (each lifts into an
/// **untagged** size-1 snapshot via `Atom::from`, which the strategy sees
/// but the wallet skips). The size hint (when available) pre-sizes the
/// equity curve.
pub fn run<Sym, S, W, I, A>(strategy: &mut S, wallet: &mut W, snapshots: I) -> RunReport<Sym>
where
    Sym: Clone + PartialEq,
    S: Strategy<Symbol = Sym, Input = Snapshot<Sym>> + ?Sized,
    W: Wallet<Sym>,
    I: IntoIterator<Item = A>,
    A: Into<Snapshot<Sym>>,
{
    drive(strategy, wallet, snapshots, DriveMode::Trade, None)
}

/// [`run`], with the strategy's own **rebalance gate forced on the final bar**
/// — the operator's "apply my new sizing now", for a deployment driving a
/// document in chunks.
///
/// A document whose `rebalance_on:` is `!never` (the default for `single:`,
/// `pairs:`, `multi:` and `portfolio:`) re-reads its `sizing:` only on a
/// transition, so a change to the account underneath it — new leverage, a
/// deposit — reaches the *book* only when the strategy next trades of its own
/// accord, which may be never. This forces one gate-fire, so the document
/// re-sizes to what it would choose against the account as it stands.
///
/// **The final bar, not the first**: the point is to size against current
/// equity and current price, and a chunk that catches up on a long gap would
/// otherwise resize at its stale head and drift for the rest of the run.
///
/// **Through the ordinary path.** The orders are the ones the document itself
/// would issue, so they queue on a [`PaperWallet`](crate::PaperWallet) and fill
/// at the next chunk's `open`, and route straight to the broker on a live
/// venue. This is not a [`Closeout`]: nothing is settled synchronously here,
/// because a rebalance is not terminal — the run continues, and booking it at
/// the last bar's close would manufacture a fill at a price the market never
/// offered.
///
/// `hold` names symbols to leave alone — the same symbols a
/// [`Closeout::Hold`] is about to drive to absolute targets, which would
/// otherwise undo this bar's resize (and cost a real round trip doing it on a
/// live venue).
///
/// Nothing happens on a bar the strategy is not
/// [`ready`](crate::Strategy::is_ready) for, or after
/// [`ruin`](RunReport::ruin_bar) — the trade step is skipped wholesale in both
/// cases, and forcing a gate does not reach past that. The report still
/// describes the bars that did go by, which is why that stays silent rather
/// than failing; with **no** bars at all there is no such report, and
/// [`rebalance_now`] refuses instead.
///
/// An empty `snapshots` is therefore not this function's case: it has no final
/// bar to arm around, and the driver routes it to [`rebalance_now`].
pub fn run_rebalancing<Sym, S, W>(
    strategy: &mut S,
    wallet: &mut W,
    snapshots: &[Snapshot<Sym>],
    hold: &[Sym],
) -> RunReport<Sym>
where
    Sym: Clone + PartialEq,
    S: Strategy<Symbol = Sym, Input = Snapshot<Sym>> + ?Sized,
    W: Wallet<Sym>,
{
    let force = snapshots.len().checked_sub(1).map(|last| (last, hold));
    drive(
        strategy,
        wallet,
        snapshots.iter().cloned(),
        DriveMode::Trade,
        force,
    )
}

/// Force the document's rebalance gate **between bars** — the bar-less twin of
/// [`run_rebalancing`], for an operator who is saying *now*.
///
/// [`run_rebalancing`] arms the gate around the final bar's
/// [`trade`](crate::Strategy::trade), so it needs a bar to hang the instruction
/// on. A deployment driven on a cadence has none: between one close and the
/// next there is no snapshot to pass, and the same call with an empty stream
/// used to drive nothing and report success. The operator then waited a full
/// cadence for a bar to close before the instruction reached the engine at all,
/// and a second for the fill — two hours on a `1h` deployment, two days on
/// `1d`. For a re-levering instruction on real money that is the wrong
/// semantics.
///
/// So this fires the gate against the account **as it already stands**: the
/// marks the wallet is carrying, the equity they imply, and the indicator
/// values the strategy was restored with. Nothing is consumed and nothing
/// advances — no snapshot, no [`update`](crate::Strategy::update), no bar
/// counter, no equity point. The strategy is left describing exactly the bar it
/// described on entry, so the [`RunState`](crate::spec::RunState) captured
/// after this resumes as if the rebalance had never happened — except for the
/// orders it queued, which live in the wallet.
///
/// **It re-runs the last bar's decision, not a fresh one.** `trade` reads
/// values, and every one of them still holds what the previous chunk's final
/// bar left there. An entry signal that fired then and has not filled yet fires
/// again here, at the size the account justifies now — which is the instruction
/// ("size against the account as it stands"), but it is a *second* `trade`
/// against one bar's worth of state rather than a new bar's worth, and
/// [`Wallet::set`] is what makes that safe: it names an absolute target, so
/// re-issuing it drives to the same place instead of adding to it.
///
/// **Empty [`RunReport`].** No bar was driven, so `equity_curve` is empty —
/// the `equity_curve.len() == snapshots.len()` invariant every consumer relies
/// on, at zero. `initial_equity` is the account's equity on entry, and
/// `rejections` carries anything a live venue refused synchronously, stamped at
/// bar `0` the way [`apply_closeout`] stamps a bar-less
/// [`Flatten`](Closeout::Flatten). No fill can appear: the orders queue on a
/// [`PaperWallet`](crate::PaperWallet) and route to the broker on a live venue,
/// and nothing fills on the bar that caused it — here as everywhere else.
///
/// **An unready strategy is refused, not ignored.** With a bar to drive, an
/// unready strategy simply does not trade and the report still describes the
/// bars that went by. Here there is no such report: driving nothing and
/// returning success is indistinguishable from "delivered, nothing to do", and
/// the caller has no way to tell that the instruction never reached the engine.
/// So it is an `Err` — the only outcome this can have other than *fired*.
pub fn rebalance_now<Sym, S, W>(
    strategy: &mut S,
    wallet: &mut W,
    hold: &[Sym],
) -> Result<RunReport<Sym>, String>
where
    Sym: Clone + PartialEq,
    S: Strategy<Symbol = Sym, Input = Snapshot<Sym>> + ?Sized,
    W: Wallet<Sym>,
{
    if !strategy.is_ready() {
        return Err(
            "the strategy has not finished warming up, so it has no sizing target to \
             rebalance to — drive the missing bars first (a resumed state carries the \
             warm-up with it), or pass the bars along with the instruction"
                .to_string(),
        );
    }
    let initial_equity = wallet.equity().0;
    // The same arm/trade/clear the driver performs around a bar's `trade`, with
    // the bar taken out. Cleared immediately after for the same reason it is
    // there: the latch must not outlive the instruction that armed it.
    strategy.force_rebalance(Some(hold));
    strategy.trade(wallet);
    strategy.force_rebalance(None);
    let rejections = wallet
        .take_rejections()
        .into_iter()
        .map(|rejection| {
            strategy.on_reject(&rejection);
            Rejected { bar: 0, rejection }
        })
        .collect();
    Ok(RunReport {
        equity_curve: Vec::new(),
        fills: Vec::new(),
        rejections,
        initial_equity,
        ruin_bar: None,
        carry_coverage: wallet.carry_coverage(),
        // Drained rather than skipped: a composite's buffers are scoped to the
        // run that produced them, and leaving one behind would hand it to the
        // next run's report. Empty for every shape here — the rows are pushed
        // by `update` and `on_fill`, neither of which runs.
        attribution: strategy.take_attribution(),
    })
}

/// Feed `snapshots` through `strategy` **without trading**: chains advance and
/// the wallet is marked to market exactly as in [`run`], but
/// [`Strategy::trade`] is never called, so no order is submitted.
///
/// The use is a *pause gap*. Bars that elapsed while a live deployment was
/// stopped have to warm the strategy's indicators, but must not book trades at
/// prices nobody could have traded at. Replaying the gap through here does the
/// first without the second, so a long-period indicator keeps its warm-up
/// across a pause instead of starting over.
///
/// Everything else is identical to [`run`], deliberately — same loop, one
/// branch. Fills still route to [`Strategy::on_fill`] (a resting order left
/// from before the pause can still trigger, and ignoring it would drift the
/// strategy's position away from the account's), and rejections still route to
/// [`Strategy::on_reject`]. No [`RunReport`] is returned: no run happened.
///
/// [`Strategy::trade`]: crate::Strategy::trade
/// [`Strategy::on_fill`]: crate::Strategy::on_fill
/// [`Strategy::on_reject`]: crate::Strategy::on_reject
pub fn warm_up<Sym, S, W, I, A>(strategy: &mut S, wallet: &mut W, snapshots: I)
where
    Sym: Clone + PartialEq,
    S: Strategy<Symbol = Sym, Input = Snapshot<Sym>> + ?Sized,
    W: Wallet<Sym>,
    I: IntoIterator<Item = A>,
    A: Into<Snapshot<Sym>>,
{
    let _ = drive(strategy, wallet, snapshots, DriveMode::WarmUpOnly, None);
}

/// Whether the shared driver loop is allowed to call
/// [`Strategy::trade`](crate::Strategy::trade).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DriveMode {
    /// A real run: trade on every bar the strategy reports ready for.
    Trade,
    /// [`warm_up`]: advance state, submit nothing.
    WarmUpOnly,
}

/// The shared body of [`run`], [`run_rebalancing`] and [`warm_up`]. See
/// [`run`] for the per-bar order of operations; `mode` gates the trade step and
/// nothing else.
///
/// `force_rebalance` is `Some((bar, hold))` to arm
/// [`Strategy::force_rebalance`] around that one bar's
/// [`trade`](crate::Strategy::trade) call — armed immediately before it and
/// cleared immediately after, so the override cannot outlive the bar it was
/// issued for even if the same strategy handle is driven again.
fn drive<Sym, S, W, I, A>(
    strategy: &mut S,
    wallet: &mut W,
    snapshots: I,
    mode: DriveMode,
    force_rebalance: Option<(usize, &[Sym])>,
) -> RunReport<Sym>
where
    Sym: Clone + PartialEq,
    S: Strategy<Symbol = Sym, Input = Snapshot<Sym>> + ?Sized,
    W: Wallet<Sym>,
    I: IntoIterator<Item = A>,
    A: Into<Snapshot<Sym>>,
{
    let initial_equity = wallet.equity().0;
    let iter = snapshots.into_iter();
    let (lower, _) = iter.size_hint();
    let mut equity_curve = Vec::with_capacity(lower);
    let mut fills: Vec<Fill<Sym>> = Vec::new();
    let mut rejections: Vec<Rejected<Sym>> = Vec::new();
    let mut ruin_bar: Option<usize> = None;

    /// Drain the wallet's failure stream, route each entry to the strategy, and
    /// record it against `bar`.
    macro_rules! drain_rejections {
        ($bar:expr) => {
            for rejection in wallet.take_rejections() {
                strategy.on_reject(&rejection);
                rejections.push(Rejected {
                    bar: $bar,
                    rejection,
                });
            }
        };
    }

    // Reused across bars so the per-bar collection is not an allocation per bar.
    let mut bar_candles: Vec<(Sym, crate::Candle)> = Vec::new();
    for (bar, snap) in iter.enumerate() {
        let snap: Snapshot<Sym> = snap.into();
        // Price the wallet for every tagged entry in the snapshot — one
        // `wallet.update(sym, candle)` call per symbol that ticked this bar.
        // The wallet returns any fills booked on that call (queued market
        // orders filling at this bar's `open`, plus resting protective legs
        // this candle's `[low, high]` triggered), routed through the
        // strategy's `on_fill` and collected into the blotter.
        // Collected and handed over as one bar rather than fed symbol by
        // symbol: the wallet has to mark every symbol before it prices any fill
        // (a `value_frac` sized against a half-marked account reads this bar's
        // close for the symbols already fed — lookahead), and it has to settle
        // this bar's sales before its purchases (a rotation is funded by its own
        // proceeds). Neither is expressible one symbol at a time. See
        // [`Wallet::advance`].
        bar_candles.clear();
        for (sym, _freq, atom) in snap.iter() {
            let Some(sym) = sym else { continue };
            // An overlay-only series carries no price, so it is not something
            // the wallet can mark a position against. Skipping is the whole
            // reason `candle` is optional: a synthesised zero would mark the
            // position to nothing, and a `NaN` would pass every `close <= 0.0`
            // guard and poison the equity curve without a single error.
            // Hand the wallet the whole atom before anything is priced: a cost
            // model whose rate is *data* (a perpetual's funding, which changes
            // every settlement and flips sign) reads its column here. The
            // default is to ignore it, so this is a no-op for every wallet that
            // prices nothing off the side channels.
            wallet.observe(sym, atom);
            let Some(candle) = atom.candle else { continue };
            bar_candles.push((sym.clone(), candle));
        }
        for fill in wallet.advance(&bar_candles) {
            strategy.on_fill(&fill);
            fills.push(Fill { bar, order: fill });
        }
        // Refusals booked while pricing — a queued market order that turned out
        // infeasible at this bar's open, or a protective leg that triggered but
        // could not be booked. Routed before update(), like the fills alongside
        // which they occurred.
        drain_rejections!(bar);
        // Drain any out-of-band fills the wallet booked between bars (a live
        // venue reports fills asynchronously, on its own schedule and possibly
        // for a symbol that didn't tick this bar). A `PaperWallet` has none —
        // its `poll_fills` keeps the empty default — so this is a no-op for
        // backtests and the equity curve is byte-identical.
        for fill in wallet.poll_fills() {
            strategy.on_fill(&fill);
            fills.push(Fill { bar, order: fill });
        }
        strategy.update(snap);
        // update()/on_fill() always run so warm-up progresses; trade() only
        // runs once the strategy reports ready. is_ready() defaults to true,
        // so this is a no-op for strategies that don't override it.
        // `WarmUpOnly` suppresses exactly this step and nothing else.
        if mode == DriveMode::Trade && ruin_bar.is_none() && strategy.is_ready() {
            let forced = force_rebalance.filter(|(at, _)| *at == bar);
            if let Some((_, hold)) = forced {
                strategy.force_rebalance(Some(hold));
            }
            strategy.trade(wallet);
            if forced.is_some() {
                strategy.force_rebalance(None);
            }
            // Refusals from this bar's own submissions — a live wallet rejecting
            // synchronously. (PaperWallet accepts everything at submit time and
            // fails at fill time instead, so this drain is empty for it.)
            drain_rejections!(bar);
        }
        // Ruin check, at the bar close, after this bar's fills and trades.
        //
        // An account at `<= 0` equity cannot fund anything, and nothing may be
        // recorded past it: `(e - prev) / prev` inverts sign once `prev < 0`, so
        // a curve allowed below zero reports *further losses as gains*. The
        // curve is therefore pinned at `0.0` from here on — one entry per
        // snapshot still, as documented, but a flat terminal one.
        //
        // Liquidating is what makes that pin honest rather than cosmetic: with
        // the book left open the wallet would keep marking it and could carry
        // equity back above zero, contradicting a curve that says the account is
        // gone. `Wallet::flatten` is the same call `--flatten` makes, so each
        // leg closes through the normal cost pipeline; on a live venue at zero
        // equity there is nothing left to close and it is a no-op.
        let equity = wallet.equity().0;
        if ruin_bar.is_some() {
            equity_curve.push(0.0);
            continue;
        }
        if mode == DriveMode::Trade && equity <= 0.0 {
            ruin_bar = Some(bar);
            for fill in wallet.flatten() {
                strategy.on_fill(&fill);
                fills.push(Fill { bar, order: fill });
            }
            drain_rejections!(bar);
            equity_curve.push(0.0);
            continue;
        }
        equity_curve.push(equity);
    }

    // Taken, not read: the composite's buffers are scoped to this run the same
    // way `fills` and `equity_curve` are, so draining here is what keeps a
    // long-lived portfolio driven over repeated runs from accumulating every
    // previous run's rows. `None` for every shape that is not a composite.
    let mut attribution = strategy.take_attribution();
    // Ruin pins the per-child rows exactly as it pins the curve they sum to.
    // The portfolio pushed each row inside its own `update`, which runs before
    // this loop's ruin check and knows nothing about it — so a ruined account
    // whose marks keep moving (a short's loss is unbounded above) would
    // otherwise leave the rows tracking it against a curve reading `0.0`, and a
    // per-child return taken off one would invert sign the moment it crossed
    // zero. Same defect, same fix, one bar index apart.
    if let (Some(a), Some(bar)) = (attribution.as_mut(), ruin_bar) {
        a.pin_from(bar);
    }

    RunReport {
        equity_curve,
        fills,
        rejections,
        initial_equity,
        ruin_bar,
        carry_coverage: wallet.carry_coverage(),
        attribution,
    }
}

/// The **operator's override at a chunk boundary** — what happens to the
/// account's open positions around a run's last bar.
///
/// [`Carry`](Closeout::Carry) is the backtest's answer and the default: an open
/// position at the end of a chunk is simply still open, and the next chunk
/// resumes holding it. The rest exist for a *deployment*, where a chunk
/// boundary is a real moment in time and somebody may need the book somewhere
/// other than where the strategy's signal would leave it.
///
/// They are one knob rather than several because they contradict each other in
/// pairs, and an enum makes a contradiction unsayable instead of a runtime
/// error with a precedence rule nobody would remember.
/// [`Flatten`](Closeout::Flatten) *is* [`Hold`](Closeout::Hold) at `0.0` for
/// every open symbol; [`Rebalance`](Closeout::Rebalance) carries its own
/// `hold`, because "resize to what the document wants" and "put this symbol
/// exactly here" are one instruction about one book, not two that happen to
/// arrive together — and it is meaningless alongside `Flatten`, which would
/// only close what it had just resized.
///
/// **Three of the four are terminal, and [`apply_closeout`] is where they
/// happen** — after the last bar's [`trade`](crate::Strategy::trade), settled
/// synchronously because there is no next bar to queue against.
/// `Rebalance` is the exception and is applied by the *driver*, around that
/// same bar's `trade`: see [`run_rebalancing`].
#[derive(Debug, Clone, Default, PartialEq)]
pub enum Closeout {
    /// Leave the book exactly as the strategy left it. Positions stay open and
    /// unrealized; a state captured after this resumes holding them.
    #[default]
    Carry,
    /// Close **every** open position in the account, through the normal cost
    /// pipeline — what `run --flatten` asks for. A state captured after this
    /// holds a genuinely flat book.
    Flatten,
    /// Drive the named symbols to these **signed unit targets** and leave every
    /// other symbol alone: `0.0` closes one position, a non-zero target resizes
    /// it, and a symbol absent from the map trades exactly as the document
    /// says.
    ///
    /// The operator override — "close this one position", "hold this one at
    /// this size" — applied after the strategy has had its say on the last bar,
    /// so it wins for that chunk. It is an absolute target rather than a delta,
    /// so passing the same map again on the next chunk holds the position there
    /// instead of moving it again; that is how a standing instruction outlives
    /// the bar it was issued on, without the driver having to carry a clock.
    Hold(std::collections::HashMap<Symbol, crate::types::Real>),
    /// **Force the document's own rebalance gate on the final bar**, then apply
    /// `hold` exactly as [`Hold`](Closeout::Hold) does.
    ///
    /// The answer to "I raised this account's leverage — apply it now". Sizing
    /// is the document's arithmetic, evaluated against the account as it
    /// stands, so this asks the strategy for its own targets rather than
    /// naming units the way `Hold` does. See [`run_rebalancing`] for what
    /// "force the gate" means per shape and why the resulting orders queue
    /// rather than settle, and [`rebalance_now`] for the bar-less case — an
    /// operator pressing this between two closes is saying *now*, and waiting
    /// for the next bar to fire the gate is a full cadence of delay on an
    /// instruction that named no time.
    ///
    /// `hold` is usually empty. When it isn't, those symbols are held out of
    /// the forced rebalance *and* driven to their targets afterwards — the
    /// narrower instruction wins, and wins without a wasted round trip.
    Rebalance {
        /// Absolute signed unit targets, read exactly as [`Hold`](Closeout::Hold)
        /// reads its map.
        hold: std::collections::HashMap<Symbol, crate::types::Real>,
    },
}

impl Closeout {
    /// Whether this leaves the book untouched — the fast path, and what a
    /// backtest always asks for.
    pub fn is_carry(&self) -> bool {
        matches!(self, Closeout::Carry)
    }

    /// The symbols a forced rebalance must leave alone, or `None` when this
    /// closeout forces no rebalance at all — what a driver hands
    /// [`crate::Strategy::force_rebalance`].
    ///
    /// Sorted, so a multi-symbol instruction reaches the strategy in the same
    /// order every run regardless of the map's layout — the same rule
    /// [`apply_closeout`] follows for the targets themselves.
    pub fn forced_rebalance_hold(&self) -> Option<Vec<Symbol>> {
        let Closeout::Rebalance { hold } = self else {
            return None;
        };
        let mut held: Vec<Symbol> = hold.keys().cloned().collect();
        held.sort();
        Some(held)
    }

    /// The absolute signed unit targets this closeout settles once the last bar
    /// is over — the map for the two variants that carry one.
    fn targets(&self) -> Option<&std::collections::HashMap<Symbol, crate::types::Real>> {
        match self {
            Closeout::Hold(targets) | Closeout::Rebalance { hold: targets } => Some(targets),
            Closeout::Carry | Closeout::Flatten => None,
        }
    }
}

/// Apply a [`Closeout`] to the account a run has just finished driving —
/// **in the wallet**, not only in the report — so a `--flatten` run finalizes
/// its open trades into the blotter (and thus the trade-level metrics via
/// [`reconstruct_trades`](crate::metrics::reconstruct_trades)).
///
/// Delegates to [`Wallet::flatten`] / [`Wallet::settle_position`], so each leg
/// goes through the account's normal execution path: costs and commission
/// apply, cash and positions move, and a real
/// [`OrderId`](crate::wallet::OrderId) is minted per leg. The fills are routed
/// to [`Strategy::on_fill`] and appended to the report at the final bar's
/// index, and any refusal (a live venue declining a close, a target the account
/// cannot fund) to [`Strategy::on_reject`] and `report.rejections`.
///
/// The **final equity point is overwritten**, not appended. Each leg closes at
/// the same mark that point was computed from, so the only change is the
/// realized cost drag — and `equity_curve.len() == snapshots.len()` is an
/// invariant [`run`] establishes and every consumer
/// (`report_slice`, `per_bar_returns`, the windowed reducers, the CLI writers)
/// relies on. Appending a point would break all of them quietly.
///
/// After this the wallet is genuinely flat: a [`RunState`](crate::spec::RunState)
/// captured from it holds no position, and resuming from that state continues
/// from a flat book rather than silently re-inheriting the closed one.
///
/// **A ruined run is left alone.** [`run`] already liquidated the book at
/// [`ruin_bar`](RunReport::ruin_bar), so there is nothing open to finalize —
/// and overwriting the final equity point would replace the pinned `0.0` with
/// the account's true negative balance, un-bounding every metric derived from
/// it.
pub fn apply_closeout<S, W>(
    strategy: &mut S,
    wallet: &mut W,
    snapshots: &[Snapshot<Symbol>],
    report: &mut RunReport<Symbol>,
    closeout: &Closeout,
) where
    S: Strategy<Symbol = Symbol, Input = Snapshot<Symbol>> + ?Sized,
    W: Wallet<Symbol>,
{
    if report.ruin_bar.is_some() || closeout.is_carry() {
        return;
    }
    let bar = snapshots.len().saturating_sub(1);
    let fills = match closeout {
        // Unreachable — returned above — but spelled out rather than
        // `unreachable!()`, so a future variant is a compile error here.
        Closeout::Carry => Vec::new(),
        Closeout::Flatten => wallet.flatten(),
        // `Rebalance`'s own half already happened, inside the last bar's
        // `trade` — all that is left of it here is the `hold` map, which it
        // reads exactly as `Hold` does.
        Closeout::Hold(_) | Closeout::Rebalance { .. } => {
            let targets = closeout.targets().expect("both arms carry targets");
            // Sorted, so a multi-symbol instruction books its legs in the same
            // order every run regardless of the map's layout — the same rule
            // `PaperWallet::flatten` follows, and for the same reason.
            let mut named: Vec<(&Symbol, &crate::types::Real)> = targets.iter().collect();
            named.sort_by(|a, b| a.0.cmp(b.0));
            named
                .into_iter()
                .flat_map(|(symbol, &target)| wallet.settle_position(symbol.clone(), target))
                .collect()
        }
    };
    for order in fills {
        strategy.on_fill(&order);
        report.fills.push(Fill { bar, order });
    }
    for rejection in wallet.take_rejections() {
        strategy.on_reject(&rejection);
        report.rejections.push(Rejected { bar, rejection });
    }
    if let Some(last) = report.equity_curve.last_mut() {
        *last = wallet.equity().0;
    }
}

/// Drive N `(strategy, wallet)` pairs over the same `snapshots` in parallel
/// and return one [`RunReport`] per pair, in the input's order.
///
/// The natural primitive for cross-strategy comparison, ensemble backtests,
/// walk-forward evaluation, and any other setting where the caller has a
/// slice of independent `(strategy, wallet)` runs to evaluate against the
/// same bar stream. Each pair owns its own wallet, so runs are fully
/// independent — no shared mutable state across workers, no locking.
///
/// The parallel iteration uses rayon; each worker picks a `(strategy,
/// wallet)` pair from `runs` and calls the plain [`run`] driver against a
/// cheap-cloning iterator over `snapshots`. Result order matches `runs`'
/// input order.
///
/// Gated behind the `parallel` Cargo feature (default-on; implied by `cli`).
/// A caller who only wants the sequential [`run`] primitive doesn't need
/// rayon and can disable the feature (`default-features = false`).
#[cfg(feature = "parallel")]
pub fn run_many<Sym, S, W>(runs: &mut [(S, W)], snapshots: &[Snapshot<Sym>]) -> Vec<RunReport<Sym>>
where
    Sym: Clone + PartialEq + Send + Sync,
    S: Strategy<Symbol = Sym, Input = Snapshot<Sym>> + Send,
    W: Wallet<Sym> + Send,
    Order<Sym>: Send,
{
    use rayon::prelude::*;
    runs.par_iter_mut()
        .map(|(strategy, wallet)| run(strategy, wallet, snapshots.iter().cloned()))
        .collect()
}

#[cfg(test)]
mod rejection_tests {
    use super::*;
    use crate::types::{Atom, Candle};
    use crate::wallet::{PaperWallet, Rejection, Side, Size, WalletError};

    fn bar(close: Real) -> Candle {
        Candle::new(close, close, close, close, 0.0)
    }

    /// Buys far more than the wallet can afford, and records what it is told.
    struct Overreacher {
        symbol: &'static str,
        seen: Vec<WalletError>,
    }

    impl Strategy for Overreacher {
        type Input = Snapshot<&'static str>;
        type Symbol = &'static str;

        fn update(&mut self, _snap: Snapshot<&'static str>) {}

        fn on_reject(&mut self, rejection: &Rejection<&'static str>) {
            self.seen.push(rejection.error);
        }

        fn trade(&self, wallet: &mut dyn Wallet<&'static str>) {
            // The shape that motivates the mechanism: the strategy discards the
            // Result, because `trade` returns (). `Size::units` is used since
            // fractional sizings shrink to fit rather than reject.
            let _ = wallet.set(self.symbol, Side::Buy, Size::units(1_000.0));
        }

        fn reset(&mut self) {}
    }

    #[test]
    fn a_refused_order_reaches_the_strategy_and_the_report() {
        let mut strategy = Overreacher {
            symbol: "X",
            seen: Vec::new(),
        };
        let mut wallet: PaperWallet<&'static str> = PaperWallet::new(100.0);
        let snaps: Vec<Snapshot<&'static str>> = [100.0, 100.0, 100.0]
            .iter()
            .map(|&p| Snapshot::single("X", Atom::new(bar(p))))
            .collect();

        let report = run(&mut strategy, &mut wallet, snaps);

        assert!(report.fills.is_empty(), "nothing could fill");
        assert_eq!(wallet.position(&"X").amount, 0.0);
        assert!(!report.rejections.is_empty(), "must be reported");
        assert!(
            report
                .rejections
                .iter()
                .all(|r| r.rejection.error == WalletError::InsufficientFunds)
        );
        // Routed to the strategy, so it can stand down rather than carry on
        // believing it is long.
        assert_eq!(
            strategy.seen.len(),
            report.rejections.len(),
            "every reported rejection reached on_reject"
        );
    }

    #[test]
    fn a_clean_run_reports_no_rejections() {
        struct Idle;
        impl Strategy for Idle {
            type Input = Snapshot<&'static str>;
            type Symbol = &'static str;
            fn update(&mut self, _snap: Snapshot<&'static str>) {}
            fn trade(&self, _wallet: &mut dyn Wallet<&'static str>) {}
            fn reset(&mut self) {}
        }
        let mut wallet: PaperWallet<&'static str> = PaperWallet::new(1_000.0);
        let snaps: Vec<Snapshot<&'static str>> = vec![Snapshot::single("X", Atom::new(bar(100.0)))];
        let report = run(&mut Idle, &mut wallet, snaps);
        assert!(report.rejections.is_empty());
        assert_eq!(report.materially_fitted(), None, "nothing traded at all");
    }

    /// A fill the wallet *fitted* is not a refusal, so it never reached
    /// `rejections` — and for a `sizing:` the account could not carry, that
    /// made a 3x request executing at 1x indistinguishable in the blotter from
    /// a signal that simply sized smaller.
    #[test]
    fn a_fitted_fill_is_reported_without_being_a_rejection() {
        /// Asks for `frac` times equity while flat, then holds — so the run
        /// books exactly one fill and the assertions are about that fill.
        struct AllIn {
            frac: Real,
        }
        impl Strategy for AllIn {
            type Input = Snapshot<&'static str>;
            type Symbol = &'static str;
            fn update(&mut self, _snap: Snapshot<&'static str>) {}
            fn trade(&self, wallet: &mut dyn Wallet<&'static str>) {
                if wallet.position(&"X").amount == 0.0 {
                    let _ = wallet.set("X", Side::Buy, Size::value_frac(self.frac));
                }
            }
            fn reset(&mut self) {}
        }
        let snaps = || -> Vec<Snapshot<&'static str>> {
            [100.0, 100.0, 100.0]
                .iter()
                .map(|&p| Snapshot::single("X", Atom::new(bar(p))))
                .collect()
        };

        // 3x equity on an unlevered account fills at a third of the request.
        let mut wallet: PaperWallet<&'static str> = PaperWallet::new(10_000.0);
        let report = run(&mut AllIn { frac: 3.0 }, &mut wallet, snaps());
        assert!(
            report.rejections.is_empty(),
            "a fitted fill is not a refusal",
        );
        let (n, worst) = report
            .materially_fitted()
            .expect("a 3x request on a 1x account is material");
        assert_eq!(n, 1);
        assert!((worst - 1.0 / 3.0).abs() < 1e-9, "worst ratio {worst}");

        // The same account carrying the same document at a ceiling that fits it
        // reports nothing — the signal is the gap, not the leverage.
        let mut wallet: PaperWallet<&'static str> = PaperWallet::new(10_000.0).with_max_gross(3.0);
        let report = run(&mut AllIn { frac: 3.0 }, &mut wallet, snaps());
        assert_eq!(report.materially_fitted(), None, "3x on a 3x account fits");

        // And an ordinary all-in is *not* material: it sheds only what
        // commission needs, which is what the threshold exists to ignore.
        let costs = crate::costs::TradingCosts::new(
            Box::new(crate::costs::PercentageCommission::new(0.001)), // 10 bps
            Box::new(crate::costs::NoSpread),
            Box::new(crate::costs::NoSlippage),
        );
        let mut wallet: PaperWallet<&'static str> = PaperWallet::with_costs(10_000.0, costs);
        let report = run(&mut AllIn { frac: 1.0 }, &mut wallet, snaps());
        assert_eq!(report.fills.len(), 1, "the all-in filled");
        assert!(
            report.fills[0].order.fill_ratio() < 1.0,
            "a costed all-in always sheds a sliver",
        );
        assert_eq!(
            report.materially_fitted(),
            None,
            "...and that sliver is not worth reporting",
        );
    }
}

#[cfg(all(test, feature = "parallel"))]
mod parallel_tests {
    use super::*;
    use crate::indicators::{BoolIndicatorExt, IndicatorExt, Sma};
    use crate::signal::Signal;
    use crate::types::{Atom, Candle};
    use crate::wallet::{PaperWallet, Side, Size};

    fn bar(close: Real) -> Candle {
        Candle::new(close, close, close, close, 0.0)
    }

    /// A minimal SMA-crossover strategy: long on fast > slow, flat when it
    /// reverses. Same shape as `PairsTrade` in the wallet tests, but on a
    /// single asset with a real signal. Kept as a compact standalone example
    /// of a hand-written [`Strategy`] that plugs into [`super::run_many`]; the
    /// crate's own [`SingleAssetStrategy`](crate::strategies::SingleAssetStrategy)
    /// now carries `Send + Sync` on its signal slots too, so it drives
    /// `run_many` directly — see `run_many_drives_single_asset_strategy`.
    struct MaCross {
        symbol: &'static str,
        long: Box<dyn Signal<Snapshot<&'static str>> + Send>,
        exit: Box<dyn Signal<Snapshot<&'static str>> + Send>,
    }

    impl MaCross {
        fn new(fast: usize, slow: usize) -> Self {
            use crate::indicators::{Close, Pick};
            let close = || Close::of(Pick::<&'static str>::new());
            Self {
                symbol: "X",
                long: Box::new(Sma::new(close(), fast).crosses_above(Sma::new(close(), slow))),
                exit: Box::new(Sma::new(close(), fast).crosses_below(Sma::new(close(), slow))),
            }
        }
    }

    impl Strategy for MaCross {
        type Input = Snapshot<&'static str>;
        type Symbol = &'static str;
        fn update(&mut self, snap: Snapshot<&'static str>) {
            self.long.update(snap.clone());
            self.exit.update(snap);
        }
        fn trade(&self, wallet: &mut dyn crate::Wallet<&'static str>) {
            let flat = wallet.position(&self.symbol).amount.abs() < 1e-9;
            if self.long.is_true() && flat {
                let _ = wallet.set(self.symbol, Side::Buy, Size::value_frac(1.0));
            } else if self.exit.is_true() && !flat {
                let _ = wallet.close(self.symbol);
            }
        }
        fn reset(&mut self) {
            self.long.reset();
            self.exit.reset();
        }
    }

    fn make_snapshots(prices: &[Real]) -> Vec<Snapshot<&'static str>> {
        prices
            .iter()
            .map(|&px| Snapshot::single("X", Atom::new(bar(px))))
            .collect()
    }

    #[test]
    fn run_many_matches_sequential_run_per_pair() {
        // Prices that produce a golden-then-death crossover.
        let prices = [
            14.0, 13.0, 12.0, 11.0, 10.0, 11.0, 13.0, 15.0, 17.0, 15.0, 12.0, 9.0, 7.0,
        ];
        let snaps = make_snapshots(&prices);

        // Sequential baseline: three independent runs.
        let mut baseline: Vec<RunReport<&'static str>> = Vec::new();
        for _ in 0..3 {
            let mut strat = MaCross::new(2, 4);
            let mut wallet: PaperWallet<&'static str> = PaperWallet::new(1_000.0);
            baseline.push(run(&mut strat, &mut wallet, snaps.iter().cloned()));
        }

        // Parallel run: three (strategy, wallet) pairs.
        let mut runs: Vec<(MaCross, PaperWallet<&'static str>)> = (0..3)
            .map(|_| (MaCross::new(2, 4), PaperWallet::new(1_000.0)))
            .collect();
        let parallel = run_many(&mut runs, &snaps);

        assert_eq!(parallel.len(), 3);
        for (b, p) in baseline.iter().zip(parallel.iter()) {
            assert_eq!(b.equity_curve, p.equity_curve);
            assert_eq!(b.initial_equity, p.initial_equity);
            assert_eq!(b.fills.len(), p.fills.len());
            for (bf, pf) in b.fills.iter().zip(p.fills.iter()) {
                assert_eq!(bf.bar, pf.bar);
                assert_eq!(bf.order.side, pf.order.side);
                assert!((bf.order.units - pf.order.units).abs() < 1e-9);
                assert!((bf.order.price - pf.order.price).abs() < 1e-9);
            }
        }
    }

    #[test]
    fn run_many_preserves_input_order() {
        // Two runs with different fast/slow — verify results come back in
        // the same slot the pair was placed in.
        let prices = [
            14.0, 13.0, 12.0, 11.0, 10.0, 11.0, 13.0, 15.0, 17.0, 15.0, 12.0, 9.0, 7.0,
        ];
        let snaps = make_snapshots(&prices);

        let mut runs: Vec<(MaCross, PaperWallet<&'static str>)> = vec![
            (MaCross::new(2, 4), PaperWallet::new(1_000.0)),
            (MaCross::new(3, 5), PaperWallet::new(1_000.0)),
        ];
        let reports = run_many(&mut runs, &snaps);
        assert_eq!(reports.len(), 2);
        // Each report matches what a sequential run would have produced for
        // its slot.
        let mut s0 = MaCross::new(2, 4);
        let mut w0: PaperWallet<&'static str> = PaperWallet::new(1_000.0);
        let seq0 = run(&mut s0, &mut w0, snaps.iter().cloned());
        let mut s1 = MaCross::new(3, 5);
        let mut w1: PaperWallet<&'static str> = PaperWallet::new(1_000.0);
        let seq1 = run(&mut s1, &mut w1, snaps.iter().cloned());
        assert_eq!(reports[0].equity_curve, seq0.equity_curve);
        assert_eq!(reports[1].equity_curve, seq1.equity_curve);
    }

    /// The payoff of the `Send + Sync` bounds on the strategy layer: the
    /// crate's own [`SingleAssetStrategy`](crate::strategies::SingleAssetStrategy)
    /// — with `Box<dyn Signal + Send + Sync>` slots — now crosses thread
    /// boundaries, so `run_many` fans a grid of the real catalogue strategies
    /// across a rayon pool without a hand-rolled stand-in. Verifies parity
    /// against the sequential `run` per slot.
    #[test]
    fn run_many_drives_single_asset_strategy() {
        use crate::strategies::SingleAssetStrategy;
        use crate::strategies::trend::ma_crossover;

        let prices = [
            14.0, 13.0, 12.0, 11.0, 10.0, 11.0, 13.0, 15.0, 17.0, 15.0, 12.0, 9.0, 7.0,
        ];
        let snaps = make_snapshots(&prices);

        // A small sweep of (fast, slow) pairs — the exact shape `optimize`
        // fans out, but over pre-built strategies rather than re-parsed specs.
        let grid = [(2usize, 4usize), (3, 5), (2, 6)];
        let mut runs: Vec<(SingleAssetStrategy<&'static str>, PaperWallet<&'static str>)> = grid
            .iter()
            .map(|&(fast, slow)| (ma_crossover("X", fast, slow), PaperWallet::new(1_000.0)))
            .collect();
        let parallel = run_many(&mut runs, &snaps);

        assert_eq!(parallel.len(), grid.len());
        for (&(fast, slow), p) in grid.iter().zip(parallel.iter()) {
            let mut strat = ma_crossover("X", fast, slow);
            let mut wallet: PaperWallet<&'static str> = PaperWallet::new(1_000.0);
            let seq = run(&mut strat, &mut wallet, snaps.iter().cloned());
            assert_eq!(seq.equity_curve, p.equity_curve);
            assert_eq!(seq.fills.len(), p.fills.len());
        }
    }
}
