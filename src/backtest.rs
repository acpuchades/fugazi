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
//! keyed collection of tagged [`Atom`]s. Each snapshot represents "all ticks
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
/// **untagged** size-1 snapshot via [`Atom::from`], which the strategy sees
/// but the wallet skips). The size hint (when available) pre-sizes the
/// equity curve.
pub fn run<Sym, S, W, I, A>(
    strategy: &mut S,
    wallet: &mut W,
    snapshots: I,
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

    for (bar, snap) in iter.enumerate() {
        let snap: Snapshot<Sym> = snap.into();
        // Price the wallet for every tagged entry in the snapshot — one
        // `wallet.update(sym, candle)` call per symbol that ticked this bar.
        // The wallet returns any fills booked on that call (queued market
        // orders filling at this bar's `open`, plus resting protective legs
        // this candle's `[low, high]` triggered), routed through the
        // strategy's `on_fill` and collected into the blotter.
        for (sym, _freq, atom) in snap.iter() {
            let Some(sym) = sym else { continue };
            for fill in wallet.update(sym.clone(), atom.candle) {
                strategy.on_fill(&fill);
                fills.push(Fill { bar, order: fill });
            }
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
        if strategy.is_ready() {
            strategy.trade(wallet);
            // Refusals from this bar's own submissions — a live wallet rejecting
            // synchronously. (PaperWallet accepts everything at submit time and
            // fails at fill time instead, so this drain is empty for it.)
            drain_rejections!(bar);
        }
        equity_curve.push(wallet.equity().0);
    }

    RunReport {
        equity_curve,
        fills,
        rejections,
        initial_equity,
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
pub fn run_many<Sym, S, W>(
    runs: &mut [(S, W)],
    snapshots: &[Snapshot<Sym>],
) -> Vec<RunReport<Sym>>
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
