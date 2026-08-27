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
    drive(strategy, wallet, snapshots, DriveMode::Trade)
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
    let _ = drive(strategy, wallet, snapshots, DriveMode::WarmUpOnly);
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

/// The shared body of [`run`] and [`warm_up`]. See [`run`] for the per-bar
/// order of operations; `mode` gates the trade step and nothing else.
fn drive<Sym, S, W, I, A>(
    strategy: &mut S,
    wallet: &mut W,
    snapshots: I,
    mode: DriveMode,
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
            strategy.trade(wallet);
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

    RunReport {
        equity_curve,
        fills,
        rejections,
        initial_equity,
        ruin_bar,
        carry_coverage: wallet.carry_coverage(),
    }
}

/// Close every position still open at the end of a run — **in the wallet**, not
/// only in the report — so a `--flatten` run finalizes its open trades into the
/// blotter (and thus the trade-level metrics via
/// [`reconstruct_trades`](crate::metrics::reconstruct_trades)).
///
/// Delegates to [`Wallet::flatten`], so each leg goes through the account's
/// normal execution path: costs and commission apply, cash and positions move,
/// and a real [`OrderId`](crate::wallet::OrderId) is minted per leg. The fills
/// are routed to [`Strategy::on_fill`] and appended to the report at the final
/// bar's index, and any refusal (a live venue declining a close) to
/// [`Strategy::on_reject`] and `report.rejections`.
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
pub fn flatten_open_positions<S, W>(
    strategy: &mut S,
    wallet: &mut W,
    snapshots: &[Snapshot<Symbol>],
    report: &mut RunReport<Symbol>,
) where
    S: Strategy<Symbol = Symbol, Input = Snapshot<Symbol>> + ?Sized,
    W: Wallet<Symbol>,
{
    if report.ruin_bar.is_some() {
        return;
    }
    let bar = snapshots.len().saturating_sub(1);
    for order in wallet.flatten() {
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
