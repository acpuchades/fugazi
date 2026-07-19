//! Integration tests for [`fugazi::Portfolio`]: composite strategy over
//! N children each trading its own [`PaperWallet`], driven by the standard
//! [`fugazi::backtest::run`] and reduced through the standard metrics
//! pipeline. Exercises fill routing, per-child equity isolation, the
//! aggregate reporting surface, and `TradingCosts::clone` (via
//! `.costs(...)` on the builder).

use fugazi::backtest;
use fugazi::costs::{FixedBpsSpread, NoSlippage, PercentageCommission, TradingCosts};
use fugazi::portfolio::policy::{EqualWeight, Fixed, WeightPolicy};
use fugazi::portfolio::{Portfolio, PortfolioBuilder};
use fugazi::prelude::*;
use fugazi::strategies::SingleAssetStrategy;
use fugazi::types::{Atom, Snapshot};
use fugazi::wallet::Order;

/// A single-symbol always-flat Candle with `close == open == high == low`,
/// unit volume — enough for the wallet to price and mark to market.
fn flat_bar(px: Real) -> Candle {
    Candle::new(px, px, px, px, 1.0)
}

/// Two synchronized single-asset snapshot streams: A rises linearly from
/// `100 → 200` over 20 bars, B stays flat at `50`. Buy-and-hold on A
/// doubles; buy-and-hold on B goes nowhere.
fn a_rising_b_flat_snapshots() -> Vec<Snapshot<&'static str>> {
    (0..20)
        .map(|i| {
            let px_a = 100.0 + 5.0 * i as Real;
            let mut snap = Snapshot::new();
            snap.push(Some("A"), None, Atom::new(flat_bar(px_a)));
            snap.push(Some("B"), None, Atom::new(flat_bar(50.0)));
            snap
        })
        .collect()
}

/// Build a portfolio of two buy-and-hold children (on A and B) with the
/// given policy and initial equity, drive it via `backtest::run`, and
/// return both the report and the portfolio (so callers can inspect
/// per-child readings).
fn run_buy_and_hold_portfolio(
    initial_equity: Real,
    policy: impl WeightPolicy,
) -> (
    Portfolio<&'static str>,
    fugazi::RunReport<&'static str>,
    fugazi::portfolio::PortfolioWallet<&'static str>,
) {
    let mut portfolio: Portfolio<&'static str> = PortfolioBuilder::default()
        .with_initial_equity(initial_equity)
        .add(
            "hold_a",
            SingleAssetStrategy::<&'static str>::with_initial_equity("A", initial_equity / 2.0)
                .long_on(
                    fugazi::indicators::Const::<Snapshot<&'static str>>::new(true),
                    fugazi::indicators::Const::<Snapshot<&'static str>>::new(false),
                ),
        )
        .add(
            "hold_b",
            SingleAssetStrategy::<&'static str>::with_initial_equity("B", initial_equity / 2.0)
                .long_on(
                    fugazi::indicators::Const::<Snapshot<&'static str>>::new(true),
                    fugazi::indicators::Const::<Snapshot<&'static str>>::new(false),
                ),
        )
        .weights(policy)
        .build();
    let mut wallet = portfolio.wallet_view();
    let report = backtest::run(&mut portfolio, &mut wallet, a_rising_b_flat_snapshots());
    (portfolio, report, wallet)
}

#[test]
fn equal_weight_splits_initial_cash_evenly() {
    let portfolio: Portfolio<&'static str> = PortfolioBuilder::default()
        .with_initial_equity(1_000.0)
        .add(
            "a",
            SingleAssetStrategy::<&'static str>::buy_and_hold("A"),
        )
        .add(
            "b",
            SingleAssetStrategy::<&'static str>::buy_and_hold("B"),
        )
        .add(
            "c",
            SingleAssetStrategy::<&'static str>::buy_and_hold("C"),
        )
        .weights(EqualWeight)
        .build();
    let wallet = portfolio.wallet_view();
    assert_eq!(portfolio.child_count(), 3);
    for i in 0..3 {
        assert!(
            (wallet.sub_equity(i).0 - 1_000.0 / 3.0).abs() < 1e-9,
            "sub {i} equity {} != 1/3",
            wallet.sub_equity(i).0
        );
    }
    // Aggregate equity == sum of subs.
    assert!((wallet.equity().0 - 1_000.0).abs() < 1e-9);
}

#[test]
fn fixed_weights_splits_at_the_configured_ratios() {
    let portfolio: Portfolio<&'static str> = PortfolioBuilder::default()
        .with_initial_equity(1_000.0)
        .add(
            "a",
            SingleAssetStrategy::<&'static str>::buy_and_hold("A"),
        )
        .add(
            "b",
            SingleAssetStrategy::<&'static str>::buy_and_hold("B"),
        )
        .weights(Fixed::new(vec![0.7, 0.3]))
        .build();
    let wallet = portfolio.wallet_view();
    assert!((wallet.sub_equity(0).0 - 700.0).abs() < 1e-9);
    assert!((wallet.sub_equity(1).0 - 300.0).abs() < 1e-9);
    assert!((wallet.equity().0 - 1_000.0).abs() < 1e-9);
}

#[test]
fn aggregate_equity_curve_sums_per_child_equity_across_bars() {
    let (portfolio, report, wallet) = run_buy_and_hold_portfolio(2_000.0, EqualWeight);

    // Aggregate curve has one entry per snapshot.
    assert_eq!(report.equity_curve.len(), 20);
    // Starts at 2_000 initial equity (pre-first-bar).
    assert!((report.initial_equity - 2_000.0).abs() < 1e-9);
    // Buy-and-hold both children — market orders fill at *next* bar's
    // open, so the true entry prices are:
    //   sub 0: buys A at bar 1's open = 105 (100 + 5*1), value_frac(1.0)
    //          resolves against equity-at-open ≈ 1000 → 1000/105 ≈ 9.524 units.
    //          Final equity at bar 19: 9.524 * 195 ≈ 1857.14.
    //   sub 1: buys B at bar 1's open = 50, value_frac(1.0) → 1000/50 = 20 units.
    //          Final equity at bar 19: 20 * 50 = 1000.
    // Aggregate: ~2857.14.
    let expected_a_units = 1000.0 / 105.0;
    let expected_final_a = expected_a_units * 195.0;
    let expected_final_b = 1_000.0;
    let expected_final_agg = expected_final_a + expected_final_b;

    let final_eq = *report.equity_curve.last().unwrap();
    assert!(
        (final_eq - expected_final_agg).abs() < 1e-6,
        "final aggregate equity {final_eq} != {expected_final_agg} (children: {}, {})",
        wallet.sub_equity(0).0,
        wallet.sub_equity(1).0,
    );
    // Wallet's live aggregate matches the last curve point.
    assert!((wallet.equity().0 - final_eq).abs() < 1e-9);
    // Per-child equities: rising-A sub gained ~85%, flat-B sub is flat.
    assert!((wallet.sub_equity(0).0 - expected_final_a).abs() < 1e-6);
    assert!((wallet.sub_equity(1).0 - expected_final_b).abs() < 1e-6);
    // Preserves child ordering / naming.
    assert_eq!(portfolio.child_name(0), "hold_a");
    assert_eq!(portfolio.child_name(1), "hold_b");
}

#[test]
fn on_fill_only_reaches_the_owning_child() {
    // Two children: one is a real buy-and-hold on A (will fill), the
    // other is a passive recorder that never trades. The recorder
    // should see zero fills; only the buy-and-hold owner sees its own
    // — verifies portfolio-wide OrderId namespacing and owners routing.
    let recorder_log =
        std::rc::Rc::new(std::cell::RefCell::new(Vec::<Order<&'static str>>::new()));
    struct SharedRecorder {
        log: std::rc::Rc<std::cell::RefCell<Vec<Order<&'static str>>>>,
    }
    impl Strategy for SharedRecorder {
        type Input = Snapshot<&'static str>;
        type Symbol = &'static str;
        fn update(&mut self, _snap: Snapshot<&'static str>) {}
        fn on_fill(&mut self, order: &Order<&'static str>) {
            self.log.borrow_mut().push(*order);
        }
        fn trade(&self, _wallet: &mut dyn Wallet<&'static str>) {}
        fn reset(&mut self) {
            self.log.borrow_mut().clear();
        }
    }
    let mut portfolio: Portfolio<&'static str> = PortfolioBuilder::default()
        .with_initial_equity(2_000.0)
        .add(
            "trader_a",
            SingleAssetStrategy::<&'static str>::buy_and_hold("A"),
        )
        .add(
            "passive_b",
            SharedRecorder {
                log: std::rc::Rc::clone(&recorder_log),
            },
        )
        .weights(EqualWeight)
        .build();
    let mut wallet = portfolio.wallet_view();
    let _report = backtest::run(&mut portfolio, &mut wallet, a_rising_b_flat_snapshots());

    assert!(
        recorder_log.borrow().is_empty(),
        "passive child received {} fills but never placed an order",
        recorder_log.borrow().len(),
    );
    // Sanity: child 0 (buy-and-hold on A) does trade, so its equity
    // grew from 1_000 → ~1_857 (A went from 100 → 195, entry at bar 1's
    // open = 105).
    assert!(wallet.sub_equity(0).0 > 1_500.0);
}

#[test]
fn install_costs_for_scopes_by_symbol_across_sub_wallets() {
    // Portfolio with two buy-and-hold children on A and B. Install a
    // non-zero commission bundle for A only via `install_costs_for("A", ...)`
    // — every A fill (in whichever sub-wallet) should book with commission;
    // every B fill should stay commission-free. This is the seam the CLI
    // uses to thread `--costs SYM:...` scoped overrides.
    let a_costs = TradingCosts::new(
        Box::new(PercentageCommission::new(0.001)),
        Box::new(FixedBpsSpread::new(10.0)),
        Box::new(NoSlippage),
    );
    let mut portfolio: Portfolio<&'static str> = PortfolioBuilder::default()
        .with_initial_equity(2_000.0)
        .add(
            "trader_a",
            SingleAssetStrategy::<&'static str>::buy_and_hold("A"),
        )
        .add(
            "trader_b",
            SingleAssetStrategy::<&'static str>::buy_and_hold("B"),
        )
        .weights(EqualWeight)
        .build();
    // Install A-only costs *after* build — mirrors the CLI's post-build
    // per-symbol install.
    portfolio.install_costs_for(&"A", a_costs);

    let mut wallet = portfolio.wallet_view();
    let report = backtest::run(&mut portfolio, &mut wallet, a_rising_b_flat_snapshots());

    // Every fill on A should carry commission (> 0); every fill on B
    // should stay commission-free.
    let a_fills: Vec<_> = report.fills.iter().filter(|f| f.order.symbol == "A").collect();
    let b_fills: Vec<_> = report.fills.iter().filter(|f| f.order.symbol == "B").collect();
    assert!(!a_fills.is_empty(), "expected at least one A fill");
    assert!(!b_fills.is_empty(), "expected at least one B fill");
    for f in &a_fills {
        assert!(
            f.order.commission > 0.0,
            "A fill should carry commission via install_costs_for; got {}",
            f.order.commission,
        );
    }
    for f in &b_fills {
        assert_eq!(
            f.order.commission, 0.0,
            "B fill should be commission-free; got {}",
            f.order.commission,
        );
    }
}

#[test]
fn passing_costs_bundle_clones_per_sub() {
    // Regression / smoke test for TradingCosts: Clone — the same bundle
    // installs on N sub-wallets and the fills carry non-zero commission.
    let costs = TradingCosts::new(
        Box::new(PercentageCommission::new(0.001)),
        Box::new(FixedBpsSpread::new(10.0)),
        Box::new(NoSlippage),
    );
    let mut portfolio: Portfolio<&'static str> = PortfolioBuilder::default()
        .with_initial_equity(2_000.0)
        .add(
            "a",
            SingleAssetStrategy::<&'static str>::buy_and_hold("A"),
        )
        .add(
            "b",
            SingleAssetStrategy::<&'static str>::buy_and_hold("B"),
        )
        .weights(EqualWeight)
        .costs(costs)
        .build();
    let mut wallet = portfolio.wallet_view();
    let report = backtest::run(&mut portfolio, &mut wallet, a_rising_b_flat_snapshots());
    // Every buy fill on each sub should carry non-zero commission
    // (percentage rate * notional > 0). At least one fill per sub.
    assert!(!report.fills.is_empty());
    for fill in &report.fills {
        assert!(
            fill.order.commission > 0.0,
            "expected non-zero commission on {:?}",
            fill.order,
        );
    }
}

#[test]
fn is_ready_gates_trade_until_every_child_is_ready() {
    // A portfolio with a child whose stable_period is high should keep
    // is_ready() false through the warm-up, and pass once every child
    // is settled. Buy-and-hold + a SMA-crossover strategy suffices —
    // the crossover needs at least the slow window filled.
    use fugazi::indicators::{Close, Pick, Sma};
    use fugazi::types::Selector;
    // Multi-asset snapshots — leaves must pick a symbol explicitly.
    let close_b = || Close::of(Pick::matching(Selector::by_symbol("B")));
    let strat_a = SingleAssetStrategy::<&'static str>::buy_and_hold("A"); // ready bar 0
    let strat_b = SingleAssetStrategy::<&'static str>::new("B").long_on(
        Sma::new(close_b(), 10).crosses_above(Sma::new(close_b(), 5)),
        Sma::new(close_b(), 10).crosses_below(Sma::new(close_b(), 5)),
    );

    let mut portfolio: Portfolio<&'static str> = PortfolioBuilder::default()
        .with_initial_equity(2_000.0)
        .add("a", strat_a)
        .add("b_ma", strat_b)
        .weights(EqualWeight)
        .build();

    // Freshly built, second child's SMA(10) needs 10 bars.
    assert!(!portfolio.is_ready(), "portfolio should not be ready pre-warm-up");

    // Feed enough bars through the portfolio's Strategy interface for
    // both children to warm up. Buy-and-hold is ready from bar 0; the
    // SMA-crossover needs 10 samples of the slow window plus its
    // crossover edge (which we approximate by feeding well over the
    // stable_period).
    let snaps = a_rising_b_flat_snapshots();
    for snap in snaps.iter().take(15) {
        portfolio.update(snap.clone());
    }
    assert!(
        portfolio.is_ready(),
        "portfolio should be ready after 15 bars"
    );
}

// ---------------------------------------------------------------------------
// Dynamic rebalance (rebalance_on: two-phase cash-then-positions)
// ---------------------------------------------------------------------------

/// A price series where symbol A doubles between bar 2 and bar 3 (so
/// after each child's entry order fills at bar 2's open, A's position
/// value jumps for the bar-3 rebalance to react to). B stays flat.
///
/// Bars are 1-indexed here for readability; snapshot indices are 0-based
/// in the returned `Vec` (`snap[0]` is bar 1).
fn a_step_up_b_flat_snapshots(bars: usize) -> Vec<Snapshot<&'static str>> {
    (0..bars)
        .map(|i| {
            // Bar 1..=2: A at 100. Bar 3+: A at 200. B always 100.
            let px_a = if i < 2 { 100.0 } else { 200.0 };
            let mut snap = Snapshot::new();
            snap.push(Some("A"), None, Atom::new(flat_bar(px_a)));
            snap.push(Some("B"), None, Atom::new(flat_bar(100.0)));
            snap
        })
        .collect()
}

#[test]
fn default_rebalance_gate_is_off_so_equities_drift_with_pnl() {
    // Without `.rebalance_on(...)`, the portfolio behaves exactly as the
    // pre-rebalance v1: weights govern the initial split, then per-child
    // equities drift with P&L and nothing re-syncs them.
    let (portfolio, _report, wallet) = run_buy_and_hold_portfolio(2_000.0, EqualWeight);
    // A rises 5x (100 → 195 over 20 bars), B stays flat → sub 0 equity
    // grew significantly, sub 1 didn't. They should be very different.
    let e0 = wallet.sub_equity(0).0;
    let e1 = wallet.sub_equity(1).0;
    assert!(
        e0 > 1.5 * e1,
        "expected significant divergence without rebalance; got sub_equity(0)={e0}, sub_equity(1)={e1}",
    );
    let _ = portfolio;
}

#[test]
fn cash_phase_alone_handles_a_rebalance_when_contributors_have_free_cash() {
    // Both children run buy-and-hold at 50% sizing so half their equity
    // stays as cash. A doubles on bar 3; the bar-3 rebalance's cash phase
    // has enough on the contributor side to snap everyone to 50/50 in
    // one fire — the position phase is a natural no-op (shortfall = 0).
    //
    // Post-entry (bars 2+):    A: 250 cash + 2.5 units of A
    //                          B: 250 cash + 2.5 units of B
    // Bar 3 close (A at 200):  A: 250 + 500 = 750 equity
    //                          B: 250 + 250 = 500 equity  (total 1250)
    // Target 50/50 = 625 each. A donates 125 cash; B receives 125.
    // Result: A: 125 + 500 = 625, B: 375 + 250 = 625. No fills queued.
    use fugazi::indicators::{Const, Every, Value};

    let mut portfolio: Portfolio<&'static str> = PortfolioBuilder::default()
        .with_initial_equity(1_000.0)
        .add(
            "half_a",
            SingleAssetStrategy::<&'static str>::with_initial_equity("A", 500.0)
                .long_on(
                    Const::<Snapshot<&'static str>>::new(true),
                    Const::<Snapshot<&'static str>>::new(false),
                )
                .position_sizing(Value::<Snapshot<&'static str>>::new(0.5)),
        )
        .add(
            "half_b",
            SingleAssetStrategy::<&'static str>::with_initial_equity("B", 500.0)
                .long_on(
                    Const::<Snapshot<&'static str>>::new(true),
                    Const::<Snapshot<&'static str>>::new(false),
                )
                .position_sizing(Value::<Snapshot<&'static str>>::new(0.5)),
        )
        .weights(Fixed::new(vec![0.5, 0.5]))
        .rebalance_on(Every::<Snapshot<&'static str>>::new(1))
        .build();
    let mut wallet = portfolio.wallet_view();
    // 4 bars: enter, fill, price step-up + rebalance, hold.
    let snaps = a_step_up_b_flat_snapshots(4);
    let _report = backtest::run(&mut portfolio, &mut wallet, snaps);

    let e0 = wallet.sub_equity(0).0;
    let e1 = wallet.sub_equity(1).0;
    assert!(
        (e0 - e1).abs() < 1.0,
        "cash phase alone should snap sub-equities to 50/50; got e0={e0}, e1={e1}",
    );
}

#[test]
fn position_phase_downsizes_when_contributor_has_no_free_cash() {
    // Buy-and-hold with 100% sizing → contributor has zero free cash to
    // donate. Cash phase can't cover the shortfall, so the position phase
    // queues a proportional set_position scale-down on the contributor's
    // position. Next fire cycle: the freed cash gets donated. Two fire
    // cycles hit the target.
    //
    // Bar 3 (fire): A is overweight by 250 and has 0 cash. Cash phase
    // moves nothing. Position phase queues a 25% haircut (250/1000).
    // Bar 4 open: fill lands → A holds 3.75 units + 250 cash, equity
    // still 1000. Bar 4 fire (Every::new(1)): cash phase donates 125
    // (delta at that point). Snap continues over more fires; here we
    // just verify convergence proceeds.
    use fugazi::indicators::{Const, Every};

    let mut portfolio: Portfolio<&'static str> = PortfolioBuilder::default()
        .with_initial_equity(1_000.0)
        .add(
            "full_a",
            SingleAssetStrategy::<&'static str>::with_initial_equity("A", 500.0).long_on(
                Const::<Snapshot<&'static str>>::new(true),
                Const::<Snapshot<&'static str>>::new(false),
            ),
        )
        .add(
            "full_b",
            SingleAssetStrategy::<&'static str>::with_initial_equity("B", 500.0).long_on(
                Const::<Snapshot<&'static str>>::new(true),
                Const::<Snapshot<&'static str>>::new(false),
            ),
        )
        .weights(Fixed::new(vec![0.5, 0.5]))
        .rebalance_on(Every::<Snapshot<&'static str>>::new(1))
        .build();
    let mut wallet = portfolio.wallet_view();
    let snaps = a_step_up_b_flat_snapshots(4);
    let _report = backtest::run(&mut portfolio, &mut wallet, snaps);

    let e0 = wallet.sub_equity(0).0;
    let e1 = wallet.sub_equity(1).0;
    // After the two-phase rebalance converges over multiple fires,
    // sub-equities should be at (or very close to) the target. Allow a
    // small tolerance since fills fill at open and the exact convergence
    // depends on price paths.
    assert!(
        (e0 - e1).abs() < 5.0,
        "phased rebalance should converge to target within a fire cycle; got e0={e0}, e1={e1}",
    );
}

#[test]
fn rebalance_gate_never_freezes_the_portfolio() {
    // `Const::false` gate (the default) — a full run, and no rebalance
    // ever runs; equities drift exactly as they would without the knob
    // at all.
    use fugazi::indicators::Const;

    let mut portfolio: Portfolio<&'static str> = PortfolioBuilder::default()
        .with_initial_equity(2_000.0)
        .add(
            "hold_a",
            SingleAssetStrategy::<&'static str>::buy_and_hold("A"),
        )
        .add(
            "hold_b",
            SingleAssetStrategy::<&'static str>::buy_and_hold("B"),
        )
        .weights(EqualWeight)
        .rebalance_on(Const::<Snapshot<&'static str>>::new(false))
        .build();
    let mut wallet = portfolio.wallet_view();
    let report = backtest::run(&mut portfolio, &mut wallet, a_rising_b_flat_snapshots());

    // Same result as run_buy_and_hold_portfolio's assertions — Const::false
    // is by definition a no-op gate.
    assert!(wallet.sub_equity(0).0 > 1.5 * wallet.sub_equity(1).0);
    assert!(!report.fills.is_empty());
}

// ---------------------------------------------------------------------------
// Precise numerical scenarios (mirror the two cases in the design walkthrough)
// ---------------------------------------------------------------------------

/// A one-shot Strategy that seeds a specific position on its first
/// [`trade`](Strategy::trade) call and then does nothing.  Used to
/// construct a portfolio whose sub-wallets start in specific
/// funds/position configurations for the scenario tests below.
struct SeedThenIdle {
    symbol: &'static str,
    units: Real,
    done: std::cell::Cell<bool>,
}

impl SeedThenIdle {
    fn new(symbol: &'static str, units: Real) -> Self {
        Self {
            symbol,
            units,
            done: std::cell::Cell::new(false),
        }
    }
}

impl Strategy for SeedThenIdle {
    type Input = Snapshot<&'static str>;
    type Symbol = &'static str;
    fn update(&mut self, _snap: Snapshot<&'static str>) {}
    fn trade(&self, wallet: &mut dyn Wallet<&'static str>) {
        if self.done.get() {
            return;
        }
        let _ = wallet.set_position(fugazi::wallet::Units {
            symbol: self.symbol,
            amount: self.units,
        });
        self.done.set(true);
    }
    fn reset(&mut self) {
        self.done.set(false);
    }
}

/// Scenario A: contributor at (200 cash + 300 in positions = 500 equity)
/// with target 400. Just remove 100 cash; no fills queued.
///
/// Setup: children A and B, each seeded 500 cash.
/// - A buys 3 units of X @ $100 (uses 300 cash, leaves 200 cash + 300 in
///   position = 500 equity).
/// - B stays flat (500 cash, 500 equity).
/// - Target after rebalance: aggregate 1000, weights [0.4, 0.6] → A: 400,
///   B: 600.
/// - Fire bar: delta A = -100, delta B = +100. Cash phase covers fully.
/// - Post-rebalance: A has 100 cash + 300 in position = 400. B has 600
///   cash + 0 in position = 600. No position downsize needed.
#[test]
fn scenario_a_cash_phase_only_moves_the_100_and_queues_no_fills() {
    use fugazi::indicators::{Const, Every};

    let mut portfolio: Portfolio<&'static str> = PortfolioBuilder::default()
        .with_initial_equity(1_000.0)
        .add("holds_x", SeedThenIdle::new("X", 3.0))
        // B does nothing — sits on its cash.
        .add(
            "idle",
            SingleAssetStrategy::<&'static str>::with_initial_equity("Y", 500.0).long_on(
                Const::<Snapshot<&'static str>>::new(false),
                Const::<Snapshot<&'static str>>::new(false),
            ),
        )
        .weights(Fixed::new(vec![0.4, 0.6]))
        .rebalance_on(Every::<Snapshot<&'static str>>::new(1))
        .build();
    let mut wallet = portfolio.wallet_view();

    // 4 bars at flat prices for X. Y symbol carries a price so the wallet
    // can mark it if needed, but nothing trades it.
    let snaps: Vec<Snapshot<&'static str>> = (0..4)
        .map(|_| {
            let mut s = Snapshot::new();
            s.push(Some("X"), None, Atom::new(flat_bar(100.0)));
            s.push(Some("Y"), None, Atom::new(flat_bar(100.0)));
            s
        })
        .collect();
    let report = backtest::run(&mut portfolio, &mut wallet, snaps);

    // Sub A should have equity 400, sub B should have equity 600. Tight
    // tolerance since the price is flat and there's no drift.
    assert!(
        (wallet.sub_equity(0).0 - 400.0).abs() < 0.01,
        "scenario A: expected sub_equity(0) == 400, got {}",
        wallet.sub_equity(0).0,
    );
    assert!(
        (wallet.sub_equity(1).0 - 600.0).abs() < 0.01,
        "scenario A: expected sub_equity(1) == 600, got {}",
        wallet.sub_equity(1).0,
    );
    // Only fill on the blotter is the initial entry buy — no rebalance
    // fill should ever have been queued (cash phase does all the work).
    assert_eq!(
        report.fills.len(),
        1,
        "scenario A: expected exactly 1 fill (initial entry); got {} fills",
        report.fills.len(),
    );
}

/// Scenario B: contributor at (200 cash + 300 in positions = 500 equity)
/// with target 250. Cash phase drains all 200 cash; position phase queues
/// a proportional downsize to shed the remaining 50 in equity next bar.
///
/// Setup: children A and B, each seeded 500 cash.
/// - A buys 3 units of X @ $100 (300 in position + 200 cash = 500 equity).
/// - B stays flat (500 cash, 500 equity).
/// - Target after rebalance: aggregate 1000, weights [0.25, 0.75] → A: 250,
///   B: 750.
/// - Fire bar T (bar 3): delta A = -250. Cash phase donates 200 (all cash).
///   Shortfall = 50. Position phase: invested = 300, f = 50/300 ≈ 0.1667,
///   queues set_position(3 * (1 - 0.1667)) = set_position(2.5).
/// - Bar T+1 (bar 4): fill lands at $100. A now holds 2.5 units, gained
///   50 in cash. A: 50 cash + 250 in position = 300 equity. B: 700 cash.
///   Bar T+1 rebalance fires: A donates 50 (delta = -50), B receives 50.
///   Final: A = 250, B = 750. Aligned.
#[test]
fn scenario_b_cash_drains_position_phase_queues_downsize_next_fire_converges() {
    use fugazi::indicators::{Const, Every};

    let mut portfolio: Portfolio<&'static str> = PortfolioBuilder::default()
        .with_initial_equity(1_000.0)
        .add("holds_x", SeedThenIdle::new("X", 3.0))
        .add(
            "idle",
            SingleAssetStrategy::<&'static str>::with_initial_equity("Y", 500.0).long_on(
                Const::<Snapshot<&'static str>>::new(false),
                Const::<Snapshot<&'static str>>::new(false),
            ),
        )
        .weights(Fixed::new(vec![0.25, 0.75]))
        .rebalance_on(Every::<Snapshot<&'static str>>::new(1))
        .build();
    let mut wallet = portfolio.wallet_view();

    // 5 bars: bar 1 seeds the position (order queued), bar 2 fills the
    // entry, bar 3 rebalance kicks in (cash phase drains + position phase
    // queues), bar 4 downsize fill lands + rebalance donates freed cash,
    // bar 5 hold. Prices flat throughout.
    let snaps: Vec<Snapshot<&'static str>> = (0..5)
        .map(|_| {
            let mut s = Snapshot::new();
            s.push(Some("X"), None, Atom::new(flat_bar(100.0)));
            s.push(Some("Y"), None, Atom::new(flat_bar(100.0)));
            s
        })
        .collect();
    let _report = backtest::run(&mut portfolio, &mut wallet, snaps);

    assert!(
        (wallet.sub_equity(0).0 - 250.0).abs() < 1.0,
        "scenario B: expected sub_equity(0) ≈ 250 (contributor's target), got {}",
        wallet.sub_equity(0).0,
    );
    assert!(
        (wallet.sub_equity(1).0 - 750.0).abs() < 1.0,
        "scenario B: expected sub_equity(1) ≈ 750 (receiver's target), got {}",
        wallet.sub_equity(1).0,
    );
}

#[test]
fn weight_shares_override_weight_policy_at_rebalance() {
    // Two buy-and-hold children with static Value(3) / Value(1) share
    // indicators. Rebalance every bar → aggregate equity should split
    // 75% / 25% between the two subs. Policy would otherwise be
    // EqualWeight (50/50), so this verifies the share indicators are
    // actually consulted and win.
    use fugazi::indicators::{Const, Every, Value};

    let mut portfolio: Portfolio<&'static str> = PortfolioBuilder::default()
        .with_initial_equity(1_000.0)
        .add(
            "big",
            SingleAssetStrategy::<&'static str>::with_initial_equity("A", 500.0)
                .long_on(
                    Const::<Snapshot<&'static str>>::new(true),
                    Const::<Snapshot<&'static str>>::new(false),
                )
                .position_sizing(Value::<Snapshot<&'static str>>::new(0.5)),
        )
        .add(
            "small",
            SingleAssetStrategy::<&'static str>::with_initial_equity("B", 500.0)
                .long_on(
                    Const::<Snapshot<&'static str>>::new(true),
                    Const::<Snapshot<&'static str>>::new(false),
                )
                .position_sizing(Value::<Snapshot<&'static str>>::new(0.5)),
        )
        .weight_shares(vec![
            Box::new(Value::<Snapshot<&'static str>>::new(3.0)),
            Box::new(Value::<Snapshot<&'static str>>::new(1.0)),
        ])
        .rebalance_on(Every::<Snapshot<&'static str>>::new(1))
        .build();
    let mut wallet = portfolio.wallet_view();
    // Flat prices throughout — the divergence in sub-equities comes
    // purely from the rebalance moving cash to hit the 75/25 target.
    let snaps: Vec<Snapshot<&'static str>> = (0..4)
        .map(|_| {
            let mut s = Snapshot::new();
            s.push(Some("A"), None, Atom::new(flat_bar(100.0)));
            s.push(Some("B"), None, Atom::new(flat_bar(100.0)));
            s
        })
        .collect();
    let _report = backtest::run(&mut portfolio, &mut wallet, snaps);

    // Aggregate equity 1000 → sub 0 gets 750, sub 1 gets 250.
    let e0 = wallet.sub_equity(0).0;
    let e1 = wallet.sub_equity(1).0;
    assert!(
        (e0 - 750.0).abs() < 5.0,
        "share-3 sub should hold ~750 equity; got {e0}",
    );
    assert!(
        (e1 - 250.0).abs() < 5.0,
        "share-1 sub should hold ~250 equity; got {e1}",
    );
}

#[test]
fn cash_covered_rebalance_queues_no_new_fills() {
    // A close cousin of scenario A: two children with cash headroom (50%
    // sizing) plus a price move that shifts equity. Verify the rebalance
    // fires but generates no new blotter entries beyond the two initial
    // entry fills — position phase should be a natural no-op.
    use fugazi::indicators::{Const, Every, Value};

    let mut portfolio: Portfolio<&'static str> = PortfolioBuilder::default()
        .with_initial_equity(1_000.0)
        .add(
            "half_a",
            SingleAssetStrategy::<&'static str>::with_initial_equity("A", 500.0)
                .long_on(
                    Const::<Snapshot<&'static str>>::new(true),
                    Const::<Snapshot<&'static str>>::new(false),
                )
                .position_sizing(Value::<Snapshot<&'static str>>::new(0.5)),
        )
        .add(
            "half_b",
            SingleAssetStrategy::<&'static str>::with_initial_equity("B", 500.0)
                .long_on(
                    Const::<Snapshot<&'static str>>::new(true),
                    Const::<Snapshot<&'static str>>::new(false),
                )
                .position_sizing(Value::<Snapshot<&'static str>>::new(0.5)),
        )
        .weights(Fixed::new(vec![0.5, 0.5]))
        .rebalance_on(Every::<Snapshot<&'static str>>::new(1))
        .build();
    let mut wallet = portfolio.wallet_view();
    let snaps = a_step_up_b_flat_snapshots(4);
    let report = backtest::run(&mut portfolio, &mut wallet, snaps);

    // Two initial entries → 2 fills. No rebalance-generated fills.
    assert_eq!(
        report.fills.len(),
        2,
        "cash-covered rebalance shouldn't queue any orders; got {} fills",
        report.fills.len(),
    );
}

#[test]
fn portfolio_book_tracks_aggregate_mark_to_market() {
    // The aggregate book Portfolio::book() should march in lockstep with
    // the sum of sub-wallet equities as each bar marks-to-market. Two
    // buy-and-hold children on A (rising) and B (flat) give a moving
    // aggregate we can assert against.
    let (portfolio, report, wallet) = run_buy_and_hold_portfolio(2_000.0, EqualWeight);
    let book = portfolio.book();
    // After the full run the book's marked equity should equal what the
    // aggregate wallet reads, and equal the final curve point.
    let final_agg = wallet.equity().0;
    let last_curve = *report.equity_curve.last().unwrap();
    assert!(
        (book.equity_value() - final_agg).abs() < 1e-9,
        "book equity {} != wallet equity {}",
        book.equity_value(),
        final_agg,
    );
    assert!(
        (book.equity_value() - last_curve).abs() < 1e-9,
        "book equity {} != last curve point {}",
        book.equity_value(),
        last_curve,
    );
    // Peak >= current (both trend up, so equal here — A rose monotonically).
    assert!(book.equity_peak_value() >= book.equity_value() - 1e-9);
    // Drawdown at a fresh peak is 0.
    let dd = book.drawdown::<Atom>().value().unwrap();
    assert!(dd.abs() < 1e-9, "expected 0 drawdown at fresh peak, got {dd}");
}

#[test]
fn portfolio_book_reset_returns_to_seed() {
    // After reset(), the aggregate book restores to its seed equity —
    // same rule as any other Book, verified end-to-end through the
    // portfolio surface.
    let (mut portfolio, _report, _wallet) =
        run_buy_and_hold_portfolio(2_000.0, EqualWeight);
    let book = portfolio.book();
    assert!(book.equity_value() > 2_000.0); // rose from the run
    portfolio.reset();
    assert!(
        (book.equity_value() - 2_000.0).abs() < 1e-9,
        "expected reset to seed 2000, got {}",
        book.equity_value()
    );
}

#[test]
fn weight_share_reads_aggregate_directly() {
    // The aggregate book is the default anchor for weight-share
    // templates — a template that reads `equity_peak` on the aggregate
    // book gives every child the same value, so the normalized weight
    // vector is uniform regardless of the underlying Fixed fallback's
    // 75/25 skew.
    //
    // Mirrors the mechanism PortfolioSpec::build uses in the YAML
    // pipeline: each per-child instantiation is built with a clone of
    // the aggregate book (linked to the child's book for `!at_child`).
    use fugazi::indicators::{Book, Const, Every};

    let agg_book: Book<&'static str> = Book::new(1_000.0);
    let child_a = SingleAssetStrategy::<&'static str>::with_initial_equity("A", 500.0)
        .long_on(
            Const::<Snapshot<&'static str>>::new(true),
            Const::<Snapshot<&'static str>>::new(false),
        );
    let child_b = SingleAssetStrategy::<&'static str>::with_initial_equity("B", 500.0)
        .long_on(
            Const::<Snapshot<&'static str>>::new(true),
            Const::<Snapshot<&'static str>>::new(false),
        );
    // Weight-share indicators built directly on the aggregate book —
    // both read the same value each bar.
    let share_a = agg_book.equity_peak::<Snapshot<&'static str>>();
    let share_b = agg_book.equity_peak::<Snapshot<&'static str>>();

    let mut portfolio: Portfolio<&'static str> = PortfolioBuilder::default()
        .with_initial_equity(1_000.0)
        .aggregate_book(agg_book.clone())
        .add("a", child_a)
        .add("b", child_b)
        .weights(Fixed::new(vec![0.75, 0.25]))
        .weight_shares(vec![Box::new(share_a), Box::new(share_b)])
        .rebalance_on(Every::<Snapshot<&'static str>>::new(1))
        .build();
    let mut wallet = portfolio.wallet_view();
    let snaps: Vec<Snapshot<&'static str>> = (0..4)
        .map(|_| {
            let mut s = Snapshot::new();
            s.push(Some("A"), None, Atom::new(flat_bar(100.0)));
            s.push(Some("B"), None, Atom::new(flat_bar(100.0)));
            s
        })
        .collect();
    let _report = backtest::run(&mut portfolio, &mut wallet, snaps);

    // Both weight-shares read the same aggregate value each bar, so
    // weights normalize to 50/50 (regardless of the 75/25 Fixed policy
    // fallback).
    let e0 = wallet.sub_equity(0).0;
    let e1 = wallet.sub_equity(1).0;
    assert!(
        (e0 - 500.0).abs() < 5.0 && (e1 - 500.0).abs() < 5.0,
        "aggregate-book weight shares should equalize the split; got e0={e0}, e1={e1}",
    );
}

/// Test strategy that opens *two* long positions on its first `trade` call
/// then goes idle. Lets us stage a contributor holding multiple positions
/// of different sizes so a position-phase policy has a meaningful choice.
struct SeedTwoThenIdle {
    a: (&'static str, Real),
    b: (&'static str, Real),
    done: std::cell::Cell<bool>,
}

impl SeedTwoThenIdle {
    fn new(a: (&'static str, Real), b: (&'static str, Real)) -> Self {
        Self {
            a,
            b,
            done: std::cell::Cell::new(false),
        }
    }
}

impl Strategy for SeedTwoThenIdle {
    type Input = Snapshot<&'static str>;
    type Symbol = &'static str;
    fn update(&mut self, _snap: Snapshot<&'static str>) {}
    fn trade(&self, wallet: &mut dyn Wallet<&'static str>) {
        if self.done.get() {
            return;
        }
        let _ = wallet.set_position(fugazi::wallet::Units {
            symbol: self.a.0,
            amount: self.a.1,
        });
        let _ = wallet.set_position(fugazi::wallet::Units {
            symbol: self.b.0,
            amount: self.b.1,
        });
        self.done.set(true);
    }
    fn reset(&mut self) {
        self.done.set(false);
    }
}

#[test]
fn largest_first_position_phase_touches_only_the_bigger_leg() {
    // A contributor over its target holds two positions of different
    // sizes. LargestFirst should shrink the bigger one (leaving the
    // smaller alone if the shortfall fits); Proportional would scale
    // both.
    //
    // Setup: equal-weight seed (500 cash each of two children). Child 0
    // opens 3 X @ 100 + 2 Y @ 100 → 500 invested, 0 cash, 500 equity.
    // Child 1 idle at 500. Aggregate 1000. Equal-weight target: still
    // 500 each — no rebalance yet.
    //
    // Then X pumps to 200. Child 0 equity = 3 * 200 + 2 * 100 = 800.
    // Child 1 still 500. Aggregate 1300. Target 650 each. Child 0
    // delta = -150. Cash = 0 → shortfall 150.
    //
    // Under LargestFirst: X value = 600 (biggest), Y = 200. Shortfall
    // fits in X — keep (600-150)/600 = 75% of X → target 2.25 units.
    // Y untouched at 2 units.
    use fugazi::indicators::{Const, Every};
    use fugazi::portfolio::rebalance::LargestFirst;

    let mut portfolio: Portfolio<&'static str> = PortfolioBuilder::default()
        .with_initial_equity(1_000.0)
        .add(
            "holds_x_and_y",
            SeedTwoThenIdle::new(("X", 3.0), ("Y", 2.0)),
        )
        .add(
            "idle",
            SingleAssetStrategy::<&'static str>::with_initial_equity("Z", 500.0)
                .long_on(
                    Const::<Snapshot<&'static str>>::new(false),
                    Const::<Snapshot<&'static str>>::new(false),
                ),
        )
        .weights(EqualWeight)
        .rebalance_on(Every::<Snapshot<&'static str>>::new(1))
        .position_rebalancer(LargestFirst)
        .build();
    let mut wallet = portfolio.wallet_view();
    // Bars 1-3 at $100 (seed + fill). Bars 4+ X pumps to $200 to force
    // child 0 over-target under equal weighting.
    let snaps: Vec<Snapshot<&'static str>> = (0..6)
        .enumerate()
        .map(|(bar, _)| {
            let x_px = if bar < 3 { 100.0 } else { 200.0 };
            let mut s = Snapshot::new();
            s.push(Some("X"), None, Atom::new(flat_bar(x_px)));
            s.push(Some("Y"), None, Atom::new(flat_bar(100.0)));
            s.push(Some("Z"), None, Atom::new(flat_bar(100.0)));
            s
        })
        .collect();
    let _report = backtest::run(&mut portfolio, &mut wallet, snaps);

    // Under LargestFirst, Y stays at 2 units and X shrinks. (Multiple
    // rebalance cycles refine, but Y never gets touched.)
    let y_units = wallet.position(&"Y").amount;
    let x_units = wallet.position(&"X").amount;
    assert!(
        (y_units - 2.0).abs() < 1e-6,
        "LargestFirst should leave Y at 2 units, got {y_units}"
    );
    assert!(
        x_units > 0.0 && x_units < 3.0,
        "LargestFirst should shrink X below its 3-unit seed; got {x_units}"
    );
}
