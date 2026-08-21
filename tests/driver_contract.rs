//! The per-bar contract of [`fugazi::backtest::run`].
//!
//! `run` is the one driver every surface funnels through — the CLI, the spec
//! layer, `Portfolio`, the Python bindings — and its docs spell out a precise
//! per-bar order:
//!
//! 1. price the wallet once per **tagged, priceable** entry, routing each fill
//!    to `on_fill` and into the blotter;
//! 2. drain the wallet's rejections to `on_reject`;
//! 3. drain out-of-band fills (`poll_fills`);
//! 4. `strategy.update(snap)` — **always**, so warm-up progresses;
//! 5. `strategy.trade(wallet)` — **only if `is_ready()`**;
//! 6. push `wallet.equity()` onto the curve.
//!
//! `src/backtest.rs`'s own unit tests cover the rejection stream and
//! `run_many`. What was untested is the *ordering and gating* above, which is
//! exactly the part every hand-rolled bar loop in a test file gets wrong. These
//! tests use a recording strategy that logs each callback, so the sequence is
//! asserted directly rather than inferred from an equity curve.

mod common;

use std::sync::{Arc, Mutex};

use common::bars::{DAY_MS, flat, overlay_only_atom};
use fugazi::backtest;
use fugazi::prelude::*;
use fugazi::types::Snapshot;

const SYMBOL: &str = "X";

/// One recorded callback into the strategy.
#[derive(Debug, Clone, PartialEq)]
enum Event {
    /// `update(snap)` — carries how many entries the snapshot held, so a
    /// price-less entry can be shown to still reach the strategy.
    Update {
        entries: usize,
    },
    Fill {
        side: Side,
        price: Real,
    },
    Reject,
    /// `trade(wallet)` — carries the bar count at the time, so gating is visible.
    Trade {
        bars_seen: usize,
    },
}

/// A strategy that records every callback and optionally withholds readiness.
///
/// `orders_on` names the bars (by zero-based index of its own `update` calls)
/// on which it submits a market buy.
struct Recorder {
    log: Arc<Mutex<Vec<Event>>>,
    bars_seen: usize,
    ready_from: usize,
    orders_on: Vec<usize>,
}

impl Recorder {
    fn new(ready_from: usize, orders_on: &[usize]) -> (Self, Arc<Mutex<Vec<Event>>>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                log: log.clone(),
                bars_seen: 0,
                ready_from,
                orders_on: orders_on.to_vec(),
            },
            log,
        )
    }

    fn push(&self, e: Event) {
        self.log.lock().expect("log").push(e);
    }
}

impl Strategy for Recorder {
    type Input = Snapshot<&'static str>;
    type Symbol = &'static str;

    fn update(&mut self, snap: Snapshot<&'static str>) {
        self.push(Event::Update {
            entries: snap.len(),
        });
        self.bars_seen += 1;
    }

    fn on_fill(&mut self, order: &Order<&'static str>) {
        self.push(Event::Fill {
            side: order.side,
            price: order.price,
        });
    }

    fn on_reject(&mut self, _rejection: &Rejection<&'static str>) {
        self.push(Event::Reject);
    }

    fn is_ready(&self) -> bool {
        self.bars_seen >= self.ready_from
    }

    fn trade(&self, wallet: &mut dyn Wallet<&'static str>) {
        self.push(Event::Trade {
            bars_seen: self.bars_seen,
        });
        if self.orders_on.contains(&(self.bars_seen - 1)) {
            let _ = wallet.set(SYMBOL, Side::Buy, Size::value_frac(1.0));
        }
    }

    fn reset(&mut self) {
        self.bars_seen = 0;
        self.log.lock().expect("log").clear();
    }
}

fn tagged(prices: &[Real]) -> Vec<Snapshot<&'static str>> {
    prices
        .iter()
        .map(|&p| Snapshot::single(SYMBOL, flat(p).into()))
        .collect()
}

// ---------------------------------------------------------------------------
// Readiness gating
// ---------------------------------------------------------------------------

/// `update()` runs on every bar so warm-up progresses, but `trade()` is called
/// only from the bar readiness is first reported. This is the
/// "safe defaults, opt-in overrides" invariant at the driver level.
#[test]
fn update_runs_every_bar_but_trade_only_once_ready() {
    let (mut strat, log) = Recorder::new(3, &[]);
    let mut wallet = PaperWallet::new(1_000.0);
    backtest::run(&mut strat, &mut wallet, tagged(&[10.0; 5]));

    let events = log.lock().expect("log").clone();
    let updates = events
        .iter()
        .filter(|e| matches!(e, Event::Update { .. }))
        .count();
    let trades: Vec<usize> = events
        .iter()
        .filter_map(|e| match e {
            Event::Trade { bars_seen } => Some(*bars_seen),
            _ => None,
        })
        .collect();

    assert_eq!(updates, 5, "update() must run on every bar");
    assert_eq!(
        trades,
        vec![3, 4, 5],
        "trade() must start on the bar is_ready() first holds and run every bar after"
    );
}

/// A strategy that never becomes ready never trades — and still gets a full,
/// finite equity curve, because marking to market is the wallet's job, not the
/// strategy's.
#[test]
fn a_never_ready_strategy_never_trades_but_still_gets_a_curve() {
    let (mut strat, log) = Recorder::new(usize::MAX, &[0, 1, 2]);
    let mut wallet = PaperWallet::new(1_000.0);
    let report = backtest::run(&mut strat, &mut wallet, tagged(&[10.0, 11.0, 12.0]));

    assert!(
        !log.lock()
            .expect("log")
            .iter()
            .any(|e| matches!(e, Event::Trade { .. })),
        "trade() must never be called"
    );
    assert!(report.fills.is_empty(), "no orders, so no fills");
    assert_eq!(report.equity_curve, vec![1_000.0; 3]);
}

// ---------------------------------------------------------------------------
// Ordering within a bar
// ---------------------------------------------------------------------------

/// Within a bar, fills are routed **before** `update`, and `trade` comes after
/// it. A strategy therefore sees last bar's fill reflected in its own state
/// before it decides anything this bar.
#[test]
fn fills_reach_the_strategy_before_update_and_trade_comes_after() {
    // Submit on bar 0; a PaperWallet fills it at bar 1's open.
    let (mut strat, log) = Recorder::new(0, &[0]);
    let mut wallet = PaperWallet::new(1_000.0);
    backtest::run(&mut strat, &mut wallet, tagged(&[10.0, 20.0, 30.0]));

    let events = log.lock().expect("log").clone();
    let fill_at = events
        .iter()
        .position(|e| matches!(e, Event::Fill { .. }))
        .expect("the bar-0 order should fill at bar 1");
    // Bar 1's callbacks are Update/Trade at indices after bar 0's pair, so the
    // fill must sit between bar 0's Trade and bar 1's Update.
    let bar1_update = events
        .iter()
        .enumerate()
        .filter(|(_, e)| matches!(e, Event::Update { .. }))
        .nth(1)
        .expect("bar 1 update")
        .0;
    let bar0_trade = events
        .iter()
        .position(|e| matches!(e, Event::Trade { .. }))
        .expect("bar 0 trade");
    assert!(
        bar0_trade < fill_at && fill_at < bar1_update,
        "the fill must land after bar 0's trade and before bar 1's update: {events:?}"
    );
}

/// A backtest never fills on the signal's own bar: the wallet queues market
/// moves and flushes them at the *next* bar's open. Here bar 0 closes at 10 and
/// bar 1 opens at 20, so the fill price is 20 — not 10.
#[test]
fn a_market_order_fills_at_the_next_bars_open() {
    let (mut strat, _log) = Recorder::new(0, &[0]);
    let mut wallet = PaperWallet::new(1_000.0);
    let report = backtest::run(&mut strat, &mut wallet, tagged(&[10.0, 20.0, 30.0]));

    let fill = report.fills.first().expect("one fill");
    assert_eq!(fill.bar, 1, "the fill is booked on the following bar");
    assert_eq!(fill.order.price, 20.0, "at that bar's open");
}

// ---------------------------------------------------------------------------
// What the wallet is and isn't shown
// ---------------------------------------------------------------------------

/// A price-less entry (an overlay series) is skipped for wallet pricing — a
/// synthesised zero would mark the position to nothing — but the strategy still
/// sees it in the snapshot. Asserted on both sides: the entry count reaching
/// `update` includes it, and the equity curve still tracks the priced leg.
#[test]
fn an_overlay_only_entry_is_invisible_to_the_wallet_but_visible_to_the_strategy() {
    let (mut strat, log) = Recorder::new(0, &[0]);
    let mut wallet = PaperWallet::new(1_000.0);

    let snaps: Vec<Snapshot<&'static str>> = [10.0, 20.0]
        .iter()
        .map(|&p| {
            let mut s = Snapshot::new();
            s.push(Some(SYMBOL), None, flat(p).into());
            s.push(Some("OVERLAY"), None, overlay_only_atom());
            s
        })
        .collect();
    let report = backtest::run(&mut strat, &mut wallet, snaps);

    let entries: Vec<usize> = log
        .lock()
        .expect("log")
        .iter()
        .filter_map(|e| match e {
            Event::Update { entries } => Some(*entries),
            _ => None,
        })
        .collect();
    assert_eq!(
        entries,
        vec![2, 2],
        "the strategy sees both entries, priceable or not"
    );
    assert!(
        report.fills.iter().all(|f| f.order.symbol == SYMBOL),
        "only the priceable symbol can fill: {:?}",
        report.fills
    );
}

/// An **untagged** snapshot — what a bare `Vec<Candle>` lifts into — reaches the
/// strategy but is skipped for wallet pricing, so nothing the strategy submits
/// can fill. Pinning this stops a test from being written against `Vec<Candle>`
/// and quietly measuring a flat curve.
#[test]
fn an_untagged_stream_reaches_the_strategy_but_prices_nothing() {
    let (mut strat, log) = Recorder::new(0, &[0]);
    let mut wallet = PaperWallet::new(1_000.0);
    let report = backtest::run(&mut strat, &mut wallet, vec![flat(10.0), flat(20.0)]);

    assert_eq!(
        log.lock()
            .expect("log")
            .iter()
            .filter(|e| matches!(e, Event::Update { .. }))
            .count(),
        2,
        "the strategy still sees every bar"
    );
    assert!(
        report.fills.is_empty(),
        "an untagged entry cannot be priced, so nothing fills: {:?}",
        report.fills
    );
    assert_eq!(report.equity_curve, vec![1_000.0; 2]);
}

// ---------------------------------------------------------------------------
// The report's own shape
// ---------------------------------------------------------------------------

/// One equity reading per input snapshot, and `initial_equity` captured *before*
/// the first bar — the two facts every metric reduction is computed against.
#[test]
fn the_report_has_one_equity_reading_per_bar_seeded_before_the_run() {
    let (mut strat, _log) = Recorder::new(0, &[0]);
    let mut wallet = PaperWallet::new(500.0);
    let prices = [10.0, 20.0, 30.0, 40.0];
    let report = backtest::run(&mut strat, &mut wallet, tagged(&prices));

    assert_eq!(report.equity_curve.len(), prices.len());
    assert_eq!(report.initial_equity, 500.0);
    // Bought at bar 1's open of 20 with the whole account, so equity tracks the
    // price from there: 500 at bar 0 and bar 1 (marked at the fill price), then
    // ×1.5 and ×2.0 as the price runs to 30 and 40.
    assert_eq!(report.equity_curve[0], 500.0);
    assert_eq!(report.equity_curve[2], 750.0);
    assert_eq!(report.equity_curve[3], 1_000.0);
}

/// A clean run reports no rejections, and a refused order both reaches
/// `on_reject` and lands in the report — the failure-side twin of the fill
/// stream. (`src/backtest.rs` unit-tests the routing with a stub wallet; this
/// pins it end to end against the real `PaperWallet`.)
#[test]
fn an_unaffordable_order_is_refused_and_reported() {
    struct Greedy;
    impl Strategy for Greedy {
        type Input = Snapshot<&'static str>;
        type Symbol = &'static str;
        fn update(&mut self, _snap: Snapshot<&'static str>) {}
        fn trade(&self, wallet: &mut dyn Wallet<&'static str>) {
            // Ten times the account, which the wallet cannot fund.
            let _ = wallet.set(SYMBOL, Side::Buy, Size::units(1_000.0));
        }
        fn reset(&mut self) {}
    }

    let mut wallet = PaperWallet::new(100.0);
    let report = backtest::run(&mut Greedy, &mut wallet, tagged(&[10.0, 10.0, 10.0]));
    assert!(
        !report.rejections.is_empty(),
        "an unfundable order must surface as a rejection rather than vanish"
    );
    for r in &report.rejections {
        assert_eq!(r.rejection.symbol, SYMBOL);
    }
}

/// Timestamps ride through untouched: the driver neither invents nor reorders
/// them, so calendar-driven signals read the bar they were handed.
#[test]
fn bar_timestamps_pass_through_untouched() {
    let (mut strat, _log) = Recorder::new(0, &[]);
    let mut wallet = PaperWallet::new(1_000.0);
    let snaps: Vec<Snapshot<&'static str>> = (0..3)
        .map(|i| {
            Snapshot::single(
                SYMBOL,
                Atom::with_time(flat(10.0), Timestamp(i as i64 * DAY_MS)),
            )
        })
        .collect();
    let seen: Vec<Option<Timestamp>> = snaps
        .iter()
        .map(|s| s.sole_atom_or_panic().and_then(|a| a.time))
        .collect();
    backtest::run(&mut strat, &mut wallet, snaps);

    assert_eq!(
        seen,
        vec![
            Some(Timestamp(0)),
            Some(Timestamp(DAY_MS)),
            Some(Timestamp(2 * DAY_MS))
        ]
    );
}

/// An empty stream is not an error: no bars, no fills, an empty curve — and
/// `initial_equity` still seeded, so a downstream reduction has something
/// well-formed to divide by (or to refuse).
#[test]
fn an_empty_stream_produces_an_empty_but_well_formed_report() {
    let (mut strat, log) = Recorder::new(0, &[0]);
    let mut wallet = PaperWallet::new(250.0);
    let report = backtest::run(
        &mut strat,
        &mut wallet,
        Vec::<Snapshot<&'static str>>::new(),
    );

    assert!(report.equity_curve.is_empty());
    assert!(report.fills.is_empty());
    assert!(report.rejections.is_empty());
    assert_eq!(report.initial_equity, 250.0);
    assert!(log.lock().expect("log").is_empty(), "no bars, no callbacks");
}

/// `warm_up` is `run` minus exactly one step: `trade`.
///
/// The whole point of the pause-gap facility is that it differs from a real run
/// in one respect and no other, so this asserts the callback log directly
/// rather than inferring it from an equity curve. `update` still fires on every
/// bar (that is what "warm" means), and nothing else moves.
#[test]
fn warm_up_advances_state_but_never_trades() {
    let prices = [100.0, 101.0, 102.0, 103.0];

    let (mut traded, traded_log) = Recorder::new(0, &[0, 1, 2, 3]);
    let mut traded_wallet = PaperWallet::new(1_000.0);
    backtest::run(&mut traded, &mut traded_wallet, tagged(&prices));

    let (mut warmed, warmed_log) = Recorder::new(0, &[0, 1, 2, 3]);
    let mut warmed_wallet = PaperWallet::new(1_000.0);
    backtest::warm_up(&mut warmed, &mut warmed_wallet, tagged(&prices));

    let traded_events = traded_log.lock().expect("log").clone();
    let warmed_events = warmed_log.lock().expect("log").clone();

    // Every `update` survives; every `Trade` (and so every `Fill` it caused) is
    // gone. Dropping the `Trade`/`Fill` entries from the real run's log must
    // leave precisely the warm-up's log.
    let expected: Vec<_> = traded_events
        .iter()
        .filter(|e| !matches!(e, Event::Trade { .. } | Event::Fill { .. }))
        .cloned()
        .collect();
    assert_eq!(
        warmed_events, expected,
        "warm_up must suppress trade() only"
    );
    assert_eq!(
        warmed_events.len(),
        prices.len(),
        "one update per bar, nothing else"
    );

    // And the account is untouched: same cash, no position.
    assert_eq!(warmed_wallet.funds().0, 1_000.0);
    assert!(warmed_wallet.positions().iter().all(|u| u.amount == 0.0));
    // The control did trade, so the comparison above is meaningful.
    assert!(
        traded_events
            .iter()
            .any(|e| matches!(e, Event::Fill { .. })),
        "the control run should have filled something"
    );
}

/// A fill that arrives *during* a warm-up still reaches the strategy.
///
/// A resting order left over from before a pause can trigger on a gap bar. The
/// strategy's own position/book must move with the account's or the two drift
/// apart for the rest of the run — suppressing `trade` must not suppress the
/// fill stream.
#[test]
fn warm_up_still_routes_fills_that_arrive_anyway() {
    // Bar 0 trades, so the queued buy fills at bar 1's open.
    let (mut strat, log) = Recorder::new(0, &[0]);
    let mut wallet = PaperWallet::new(1_000.0);
    backtest::run(&mut strat, &mut wallet, tagged(&[100.0]));
    assert!(
        wallet.positions().iter().all(|u| u.amount == 0.0),
        "the buy should still be queued, not filled"
    );

    // Now warm up over the next bar: no new order, but the queued one fills.
    log.lock().expect("log").clear();
    backtest::warm_up(&mut strat, &mut wallet, tagged(&[101.0]));

    let events = log.lock().expect("log").clone();
    assert!(
        events.iter().any(|e| matches!(e, Event::Fill { .. })),
        "the pre-existing queued order must still fill and route: {events:?}"
    );
    assert!(
        !events.iter().any(|e| matches!(e, Event::Trade { .. })),
        "no new orders during a warm-up: {events:?}"
    );
}
