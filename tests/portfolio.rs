//! Integration tests for [`fugazi::Portfolio`]: a composite strategy over N
//! children that netts their intents onto the one account it is handed, driven
//! by the standard [`fugazi::backtest::run`] and reduced through the standard
//! metrics pipeline. Exercises fill attribution, per-child equity isolation
//! (via the ledgers), the aggregate reporting surface, and crossing.

use fugazi::backtest;
use fugazi::costs::{FixedBpsSpread, NoSlippage, PercentageCommission, TradingCosts};
use fugazi::portfolio::policy::{EqualWeight, Fixed, WeightPolicy};
use fugazi::portfolio::{Portfolio, PortfolioBuilder};
use fugazi::prelude::*;
use fugazi::strategies::SingleAssetStrategy;
use fugazi::types::{Atom, Snapshot};
use fugazi::wallet::{Order, PaperWallet};

/// A single-symbol always-flat Candle with `close == open == high == low`,
/// unit volume — enough for the wallet to price and mark to market.
fn flat_bar(px: Real) -> Candle {
    Candle::new(px, px, px, px, 1.0)
}

/// A price-less atom — an overlay series carries values but no candle, and
/// `backtest::run` skips it for wallet pricing.
fn overlay_only_atom() -> Atom {
    Atom::overlay_only(
        OverlayInfo::new(std::sync::Arc::new(Schema::default()), Vec::new()),
        Timestamp(0),
    )
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
    policy: impl WeightPolicy + Send,
) -> (
    Portfolio<&'static str>,
    fugazi::RunReport<&'static str>,
    PaperWallet<&'static str>,
) {
    let mut portfolio: Portfolio<&'static str> = PortfolioBuilder::default()
        .with_initial_equity(initial_equity)
        .add(
            "hold_a",
            SingleAssetStrategy::<&'static str>::with_initial_equity("A", initial_equity / 2.0)
                .long_on(
                    fugazi::indicators::ValueBool::<Snapshot<&'static str>>::new(true),
                    fugazi::indicators::ValueBool::<Snapshot<&'static str>>::new(false),
                ),
        )
        .add(
            "hold_b",
            SingleAssetStrategy::<&'static str>::with_initial_equity("B", initial_equity / 2.0)
                .long_on(
                    fugazi::indicators::ValueBool::<Snapshot<&'static str>>::new(true),
                    fugazi::indicators::ValueBool::<Snapshot<&'static str>>::new(false),
                ),
        )
        .weights(policy)
        .build();
    let mut wallet = PaperWallet::new(initial_equity);
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
    let wallet: PaperWallet<&'static str> = PaperWallet::new(1_000.0);
    assert_eq!(portfolio.child_count(), 3);
    for i in 0..3 {
        assert!(
            (portfolio.sub_equity(i) - 1_000.0 / 3.0).abs() < 1e-9,
            "sub {i} equity {} != 1/3",
            portfolio.sub_equity(i)
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
    let wallet: PaperWallet<&'static str> = PaperWallet::new(1_000.0);
    assert!((portfolio.sub_equity(0) - 700.0).abs() < 1e-9);
    assert!((portfolio.sub_equity(1) - 300.0).abs() < 1e-9);
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
        portfolio.sub_equity(0),
        portfolio.sub_equity(1),
    );
    // Wallet's live aggregate matches the last curve point.
    assert!((wallet.equity().0 - final_eq).abs() < 1e-9);
    // Per-child equities: rising-A sub gained ~85%, flat-B sub is flat.
    assert!((portfolio.sub_equity(0) - expected_final_a).abs() < 1e-6);
    assert!((portfolio.sub_equity(1) - expected_final_b).abs() < 1e-6);
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
    // `Arc<Mutex<_>>`, not `Rc<RefCell<_>>`: a portfolio child must be `Send`
    // now that `Portfolio` itself is, so shared test state has to be too.
    let recorder_log =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::<Order<&'static str>>::new()));
    struct SharedRecorder {
        log: std::sync::Arc<std::sync::Mutex<Vec<Order<&'static str>>>>,
    }
    impl Strategy for SharedRecorder {
        type Input = Snapshot<&'static str>;
        type Symbol = &'static str;
        fn update(&mut self, _snap: Snapshot<&'static str>) {}
        fn on_fill(&mut self, order: &Order<&'static str>) {
            self.log.lock().unwrap().push(*order);
        }
        fn trade(&self, _wallet: &mut dyn Wallet<&'static str>) {}
        fn reset(&mut self) {
            self.log.lock().unwrap().clear();
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
                log: std::sync::Arc::clone(&recorder_log),
            },
        )
        .weights(EqualWeight)
        .build();
    let mut wallet = PaperWallet::new(2_000.0);
    let _report = backtest::run(&mut portfolio, &mut wallet, a_rising_b_flat_snapshots());

    assert!(
        recorder_log.lock().unwrap().is_empty(),
        "passive child received {} fills but never placed an order",
        recorder_log.lock().unwrap().len(),
    );
    // Sanity: child 0 (buy-and-hold on A) does trade, so its equity
    // grew from 1_000 → ~1_857 (A went from 100 → 195, entry at bar 1's
    // open = 105).
    assert!(portfolio.sub_equity(0) > 1_500.0);
}

#[test]
fn per_symbol_costs_on_the_account_scope_by_symbol() {
    // A portfolio now trades the wallet it is handed, so per-symbol costs live
    // on that account like every other shape: install an A-only bundle on the
    // wallet, and every A fill books with commission while every B fill stays
    // free. This is the seam the CLI uses for `--costs SYM:...`.
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
    let mut wallet = PaperWallet::new(2_000.0);
    wallet.set_costs_for("A", a_costs).unwrap();

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
fn account_costs_apply_to_every_child_fill() {
    // The account carries one uniform cost bundle; every child's fills into it
    // book at that rate.
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
        .build();
    let mut wallet = PaperWallet::with_costs(2_000.0, costs);
    let report = backtest::run(&mut portfolio, &mut wallet, a_rising_b_flat_snapshots());
    // Every buy fill should carry non-zero commission
    // (percentage rate * notional > 0). At least one fill per child.
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
    // A portfolio with a child whose stable_bars is high should keep
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
    // stable_bars).
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
    let (portfolio, _report, _wallet) = run_buy_and_hold_portfolio(2_000.0, EqualWeight);
    // A rises 5x (100 → 195 over 20 bars), B stays flat → sub 0 equity
    // grew significantly, sub 1 didn't. They should be very different.
    let e0 = portfolio.sub_equity(0);
    let e1 = portfolio.sub_equity(1);
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
    use fugazi::indicators::{ValueBool, Every, Value};

    let mut portfolio: Portfolio<&'static str> = PortfolioBuilder::default()
        .with_initial_equity(1_000.0)
        .add(
            "half_a",
            SingleAssetStrategy::<&'static str>::with_initial_equity("A", 500.0)
                .long_on(
                    ValueBool::<Snapshot<&'static str>>::new(true),
                    ValueBool::<Snapshot<&'static str>>::new(false),
                )
                .position_sizing(Value::<Snapshot<&'static str>>::new(0.5)),
        )
        .add(
            "half_b",
            SingleAssetStrategy::<&'static str>::with_initial_equity("B", 500.0)
                .long_on(
                    ValueBool::<Snapshot<&'static str>>::new(true),
                    ValueBool::<Snapshot<&'static str>>::new(false),
                )
                .position_sizing(Value::<Snapshot<&'static str>>::new(0.5)),
        )
        .weights(Fixed::new(vec![0.5, 0.5]))
        .rebalance_on(Every::<Snapshot<&'static str>>::new(1))
        .build();
    let mut wallet = PaperWallet::new(1_000.0);
    // 4 bars: enter, fill, price step-up + rebalance, hold.
    let snaps = a_step_up_b_flat_snapshots(4);
    let _report = backtest::run(&mut portfolio, &mut wallet, snaps);

    let e0 = portfolio.sub_equity(0);
    let e1 = portfolio.sub_equity(1);
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
    use fugazi::indicators::{ValueBool, Every};

    let mut portfolio: Portfolio<&'static str> = PortfolioBuilder::default()
        .with_initial_equity(1_000.0)
        .add(
            "full_a",
            SingleAssetStrategy::<&'static str>::with_initial_equity("A", 500.0).long_on(
                ValueBool::<Snapshot<&'static str>>::new(true),
                ValueBool::<Snapshot<&'static str>>::new(false),
            ),
        )
        .add(
            "full_b",
            SingleAssetStrategy::<&'static str>::with_initial_equity("B", 500.0).long_on(
                ValueBool::<Snapshot<&'static str>>::new(true),
                ValueBool::<Snapshot<&'static str>>::new(false),
            ),
        )
        .weights(Fixed::new(vec![0.5, 0.5]))
        .rebalance_on(Every::<Snapshot<&'static str>>::new(1))
        .build();
    let mut wallet = PaperWallet::new(1_000.0);
    let snaps = a_step_up_b_flat_snapshots(4);
    let _report = backtest::run(&mut portfolio, &mut wallet, snaps);

    let e0 = portfolio.sub_equity(0);
    let e1 = portfolio.sub_equity(1);
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
    // `ValueBool::false` gate (the default) — a full run, and no rebalance
    // ever runs; equities drift exactly as they would without the knob
    // at all.
    use fugazi::indicators::ValueBool;

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
        .rebalance_on(ValueBool::<Snapshot<&'static str>>::new(false))
        .build();
    let mut wallet = PaperWallet::new(2_000.0);
    let report = backtest::run(&mut portfolio, &mut wallet, a_rising_b_flat_snapshots());

    // Same result as run_buy_and_hold_portfolio's assertions — ValueBool::false
    // is by definition a no-op gate.
    assert!(portfolio.sub_equity(0) > 1.5 * portfolio.sub_equity(1));
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
    use fugazi::indicators::{ValueBool, Every};

    let mut portfolio: Portfolio<&'static str> = PortfolioBuilder::default()
        .with_initial_equity(1_000.0)
        .add("holds_x", SeedThenIdle::new("X", 3.0))
        // B does nothing — sits on its cash.
        .add(
            "idle",
            SingleAssetStrategy::<&'static str>::with_initial_equity("Y", 500.0).long_on(
                ValueBool::<Snapshot<&'static str>>::new(false),
                ValueBool::<Snapshot<&'static str>>::new(false),
            ),
        )
        .weights(Fixed::new(vec![0.4, 0.6]))
        .rebalance_on(Every::<Snapshot<&'static str>>::new(1))
        .build();
    let mut wallet = PaperWallet::new(1_000.0);

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
        (portfolio.sub_equity(0) - 400.0).abs() < 0.01,
        "scenario A: expected sub_equity(0) == 400, got {}",
        portfolio.sub_equity(0),
    );
    assert!(
        (portfolio.sub_equity(1) - 600.0).abs() < 0.01,
        "scenario A: expected sub_equity(1) == 600, got {}",
        portfolio.sub_equity(1),
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
    use fugazi::indicators::{ValueBool, Every};

    let mut portfolio: Portfolio<&'static str> = PortfolioBuilder::default()
        .with_initial_equity(1_000.0)
        .add("holds_x", SeedThenIdle::new("X", 3.0))
        .add(
            "idle",
            SingleAssetStrategy::<&'static str>::with_initial_equity("Y", 500.0).long_on(
                ValueBool::<Snapshot<&'static str>>::new(false),
                ValueBool::<Snapshot<&'static str>>::new(false),
            ),
        )
        .weights(Fixed::new(vec![0.25, 0.75]))
        .rebalance_on(Every::<Snapshot<&'static str>>::new(1))
        .build();
    let mut wallet = PaperWallet::new(1_000.0);

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
        (portfolio.sub_equity(0) - 250.0).abs() < 1.0,
        "scenario B: expected sub_equity(0) ≈ 250 (contributor's target), got {}",
        portfolio.sub_equity(0),
    );
    assert!(
        (portfolio.sub_equity(1) - 750.0).abs() < 1.0,
        "scenario B: expected sub_equity(1) ≈ 750 (receiver's target), got {}",
        portfolio.sub_equity(1),
    );
}

#[test]
fn weight_shares_override_weight_policy_at_rebalance() {
    // Two buy-and-hold children with static Value(3) / Value(1) share
    // indicators. Rebalance every bar → aggregate equity should split
    // 75% / 25% between the two subs. Policy would otherwise be
    // EqualWeight (50/50), so this verifies the share indicators are
    // actually consulted and win.
    use fugazi::indicators::{ValueBool, Every, Value};

    let mut portfolio: Portfolio<&'static str> = PortfolioBuilder::default()
        .with_initial_equity(1_000.0)
        .add(
            "big",
            SingleAssetStrategy::<&'static str>::with_initial_equity("A", 500.0)
                .long_on(
                    ValueBool::<Snapshot<&'static str>>::new(true),
                    ValueBool::<Snapshot<&'static str>>::new(false),
                )
                .position_sizing(Value::<Snapshot<&'static str>>::new(0.5)),
        )
        .add(
            "small",
            SingleAssetStrategy::<&'static str>::with_initial_equity("B", 500.0)
                .long_on(
                    ValueBool::<Snapshot<&'static str>>::new(true),
                    ValueBool::<Snapshot<&'static str>>::new(false),
                )
                .position_sizing(Value::<Snapshot<&'static str>>::new(0.5)),
        )
        .weight_shares(vec![
            Box::new(Value::<Snapshot<&'static str>>::new(3.0)),
            Box::new(Value::<Snapshot<&'static str>>::new(1.0)),
        ])
        .rebalance_on(Every::<Snapshot<&'static str>>::new(1))
        .build();
    let mut wallet = PaperWallet::new(1_000.0);
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
    let e0 = portfolio.sub_equity(0);
    let e1 = portfolio.sub_equity(1);
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
    use fugazi::indicators::{ValueBool, Every, Value};

    let mut portfolio: Portfolio<&'static str> = PortfolioBuilder::default()
        .with_initial_equity(1_000.0)
        .add(
            "half_a",
            SingleAssetStrategy::<&'static str>::with_initial_equity("A", 500.0)
                .long_on(
                    ValueBool::<Snapshot<&'static str>>::new(true),
                    ValueBool::<Snapshot<&'static str>>::new(false),
                )
                .position_sizing(Value::<Snapshot<&'static str>>::new(0.5)),
        )
        .add(
            "half_b",
            SingleAssetStrategy::<&'static str>::with_initial_equity("B", 500.0)
                .long_on(
                    ValueBool::<Snapshot<&'static str>>::new(true),
                    ValueBool::<Snapshot<&'static str>>::new(false),
                )
                .position_sizing(Value::<Snapshot<&'static str>>::new(0.5)),
        )
        .weights(Fixed::new(vec![0.5, 0.5]))
        .rebalance_on(Every::<Snapshot<&'static str>>::new(1))
        .build();
    let snaps = a_step_up_b_flat_snapshots(4);
    let report = portfolio.run(snaps);

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
    // pipeline: each per-child instantiation is built with the child's
    // own book as the strategy book, and the aggregate book passed as
    // the `portfolio_book` build argument (so `source: !portfolio_book`
    // resolves to the aggregate).
    use fugazi::indicators::{Book, ValueBool, Every};

    let agg_book: Book<&'static str> = Book::new(1_000.0);
    let child_a = SingleAssetStrategy::<&'static str>::with_initial_equity("A", 500.0)
        .long_on(
            ValueBool::<Snapshot<&'static str>>::new(true),
            ValueBool::<Snapshot<&'static str>>::new(false),
        );
    let child_b = SingleAssetStrategy::<&'static str>::with_initial_equity("B", 500.0)
        .long_on(
            ValueBool::<Snapshot<&'static str>>::new(true),
            ValueBool::<Snapshot<&'static str>>::new(false),
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
    let mut wallet = PaperWallet::new(1_000.0);
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
    let e0 = portfolio.sub_equity(0);
    let e1 = portfolio.sub_equity(1);
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
    use fugazi::indicators::{ValueBool, Every};
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
                    ValueBool::<Snapshot<&'static str>>::new(false),
                    ValueBool::<Snapshot<&'static str>>::new(false),
                ),
        )
        .weights(EqualWeight)
        .rebalance_on(Every::<Snapshot<&'static str>>::new(1))
        .position_rebalancer(LargestFirst)
        .build();
    let mut wallet = PaperWallet::new(1_000.0);
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

/// The point of `Arc<Mutex<_>>` over `Rc<RefCell<_>>`: a portfolio can cross a
/// thread boundary. Before, `Portfolio` was `!Send`, so an ensemble of
/// portfolios could only be evaluated serially and the type could never be
/// handed to a worker pool.
#[test]
fn a_portfolio_can_be_driven_from_another_thread() {
    let bars = [10.0, 11.0, 12.0, 13.0];
    let snaps: Vec<Snapshot<&'static str>> = bars
        .iter()
        .map(|&p| {
            let mut s = Snapshot::new();
            s.push(Some("A"), None, Atom::new(Candle::new(p, p, p, p, 100.0)));
            s
        })
        .collect();

    let handle = std::thread::spawn(move || {
        let mut portfolio: Portfolio<&'static str> = PortfolioBuilder::default()
            .with_initial_equity(1_000.0)
            .add(
                "hold_a",
                SingleAssetStrategy::<&'static str>::buy_and_hold("A"),
            )
            .weights(EqualWeight)
            .build();
        let mut wallet = PaperWallet::new(1_000.0);
        let report = fugazi::backtest::run(&mut portfolio, &mut wallet, snaps);
        *report.equity_curve.last().unwrap()
    });

    let final_equity = handle.join().expect("the worker must not panic");
    // Bought at bar 1's open (11) with 1000, held to 13.
    assert!(final_equity > 1_000.0, "equity {final_equity} should have grown");
}

// ---------------------------------------------------------------------------
// Rejection routing, the wallet-pairing guard, and the per-child wallet seam.
// ---------------------------------------------------------------------------

/// Shared per-child log of everything the driver routed back to a child.
type RejectLog = std::sync::Arc<std::sync::Mutex<Vec<Rejection<&'static str>>>>;

/// A child that submits one fixed order per bar and records every rejection
/// routed back to it. `Arc<Mutex<_>>` rather than `Rc<RefCell<_>>` because a
/// portfolio child must be `Send`.
struct Submitter {
    log: RejectLog,
    /// What to submit each bar. `None` submits nothing.
    order: Option<(&'static str, Side, Size)>,
}

impl Strategy for Submitter {
    type Input = Snapshot<&'static str>;
    type Symbol = &'static str;
    fn update(&mut self, _snap: Snapshot<&'static str>) {}
    fn on_reject(&mut self, rejection: &Rejection<&'static str>) {
        self.log.lock().unwrap().push(*rejection);
    }
    fn trade(&self, wallet: &mut dyn Wallet<&'static str>) {
        if let Some((sym, side, size)) = self.order {
            let _ = wallet.set(sym, side, size);
        }
    }
    fn reset(&mut self) {
        self.log.lock().unwrap().clear();
    }
}

fn submitter(log: &RejectLog, order: Option<(&'static str, Side, Size)>) -> Submitter {
    Submitter {
        log: std::sync::Arc::clone(log),
        order,
    }
}

#[test]
fn child_hard_cap_rejections_reach_the_child_not_the_report() {
    // A child that asks to spend past its ledger slice is refused *inside* the
    // portfolio (`record_intent`), not by the account — the account may hold a
    // sibling's cash. So the refusal reaches the child via `on_reject`, but not
    // the run report: the documented consequence of the portfolio no longer
    // owning a wallet the driver could drain.
    let log: RejectLog = Default::default();
    let mut portfolio: Portfolio<&'static str> = PortfolioBuilder::default()
        .with_initial_equity(1_000.0)
        // Asks for a billion units of A against ~500 of ledger cash — refused
        // by the child's own ledger cap, every bar.
        .add("greedy", submitter(&log, Some(("A", Side::Buy, Size::units(1e9)))))
        .add("idle", submitter(&Default::default(), None))
        .weights(EqualWeight)
        .build();
    let report = portfolio.run(a_rising_b_flat_snapshots());

    assert!(
        report.rejections.is_empty(),
        "child hard-cap refusals stay off the account-level report",
    );
    let entries = log.lock().unwrap();
    assert!(!entries.is_empty(), "the greedy child must hear its own refusals");
    assert!(
        entries.iter().all(|r| r.error == WalletError::InsufficientFunds),
        "expected every refusal to be InsufficientFunds",
    );
}

#[test]
fn a_rejection_reaches_only_the_owning_child() {
    // Mirrors `on_fill_only_reaches_the_owning_child` on the failure side.
    let greedy_log: RejectLog = Default::default();
    let innocent_log: RejectLog = Default::default();
    let mut portfolio: Portfolio<&'static str> = PortfolioBuilder::default()
        .with_initial_equity(1_000.0)
        .add(
            "greedy",
            submitter(&greedy_log, Some(("A", Side::Buy, Size::units(1e9)))),
        )
        // Affordable, so it never refuses — and must not be told about its
        // sibling's refusals.
        .add(
            "innocent",
            submitter(&innocent_log, Some(("B", Side::Buy, Size::value_frac(0.5)))),
        )
        .weights(EqualWeight)
        .build();
    let _ = portfolio.run(a_rising_b_flat_snapshots());

    assert!(!greedy_log.lock().unwrap().is_empty());
    assert!(
        innocent_log.lock().unwrap().is_empty(),
        "the innocent child was handed {} of its sibling's rejections",
        innocent_log.lock().unwrap().len(),
    );
}

#[test]
fn a_child_running_past_its_slice_after_partial_fills_reaches_the_child() {
    // The child clears its ledger cap on the first bars (buying against the low
    // close), but as the price gaps up its partial fills drain the slice and a
    // later bar's intent is refused by the ledger cap. That refusal reaches the
    // child via `on_reject` (child-level, so not in the run report).
    let log: RejectLog = Default::default();
    let mut portfolio: Portfolio<&'static str> = PortfolioBuilder::default()
        .with_initial_equity(1_000.0)
        // 45 units cost 450 at the last close of 10 — comfortably within the
        // child's 1_000, so pre-flight passes and the order is acked. At the
        // gapped open of 100 the same 45 units cost 4_500 and can't be paid for.
        .add("gapped", submitter(&log, Some(("A", Side::Buy, Size::units(45.0)))))
        .weights(EqualWeight)
        .build();

    // ...then gap the open 10x above that close, so the queued order can no
    // longer be paid for when it fills.
    let snaps: Vec<Snapshot<&'static str>> = (0..4)
        .map(|i| {
            let mut snap = Snapshot::new();
            let candle = if i == 0 {
                flat_bar(10.0)
            } else {
                // open 100, rest of the bar back at 10 — only the open matters
                // for a queued market fill.
                Candle::new(100.0, 100.0, 10.0, 10.0, 1.0)
            };
            snap.push(Some("A"), None, Atom::new(candle));
            snap
        })
        .collect();
    let _report = portfolio.run(snaps);

    assert!(
        log.lock()
            .unwrap()
            .iter()
            .any(|r| r.error == WalletError::InsufficientFunds),
        "the child must be told when it runs past its own slice",
    );
}

// The two mis-pairing-guard `#[should_panic]` tests are gone: a portfolio now
// trades whatever wallet the driver hands it, so driving it with any
// `PaperWallet` (or a live account) is the supported path, not a panic.

#[test]
fn an_overlay_only_stream_produces_a_full_equity_curve() {
    // A stream of price-less (overlay) atoms carries nothing to price; a run
    // over it still produces one equity-curve point per bar.
    let mut portfolio: Portfolio<&'static str> = PortfolioBuilder::default()
        .with_initial_equity(1_000.0)
        .add("idle", submitter(&Default::default(), None))
        .weights(EqualWeight)
        .build();
    let snaps: Vec<Snapshot<&'static str>> = (0..5)
        .map(|_| {
            let mut snap = Snapshot::new();
            snap.push(Some("A"), None, overlay_only_atom());
            snap
        })
        .collect();
    let report = portfolio.run(snaps);
    assert_eq!(report.equity_curve.len(), 5);
}

#[test]
fn an_overlay_only_bar_mid_run_is_handled() {
    // A priceable bar followed by an overlay-only one: the overlay bar simply
    // carries no mark, and the run continues.
    let mut portfolio: Portfolio<&'static str> = PortfolioBuilder::default()
        .with_initial_equity(1_000.0)
        .add("a", SingleAssetStrategy::<&'static str>::buy_and_hold("A"))
        .weights(EqualWeight)
        .build();
    let snaps: Vec<Snapshot<&'static str>> = (0..6)
        .map(|i| {
            let mut snap = Snapshot::new();
            let atom = if i == 3 {
                overlay_only_atom() // no candle on this bar
            } else {
                Atom::new(flat_bar(100.0))
            };
            snap.push(Some("A"), None, atom);
            snap
        })
        .collect();
    let report = portfolio.run(snaps);
    assert_eq!(report.equity_curve.len(), 6);
}

#[test]
#[should_panic(expected = "cannot be a child of a Portfolio")]
fn a_nested_portfolio_is_refused_at_build() {
    // Compiles (a Portfolio satisfies `add`'s bounds) but can never work:
    // only the outer composite wallet receives bars, so the inner
    // portfolio's sub-wallets would never be priced.
    let inner: Portfolio<&'static str> = PortfolioBuilder::default()
        .with_initial_equity(500.0)
        .add("a", SingleAssetStrategy::<&'static str>::buy_and_hold("A"))
        .weights(EqualWeight)
        .build();
    let _ = PortfolioBuilder::default()
        .with_initial_equity(1_000.0)
        .add("nested", inner)
        .weights(EqualWeight)
        .build();
}

#[test]
fn portfolio_run_matches_hand_paired_backtest_run() {
    // `Portfolio::run` must be exactly the hand-paired spelling — a fresh
    // `PaperWallet` at the portfolio's seed driven through `backtest::run`.
    let build = || -> Portfolio<&'static str> {
        PortfolioBuilder::default()
            .with_initial_equity(2_000.0)
            .add("a", SingleAssetStrategy::<&'static str>::buy_and_hold("A"))
            .add("b", SingleAssetStrategy::<&'static str>::buy_and_hold("B"))
            .weights(EqualWeight)
            .build()
    };

    let mut hand_paired = build();
    let mut wallet = PaperWallet::new(2_000.0);
    let expected = backtest::run(&mut hand_paired, &mut wallet, a_rising_b_flat_snapshots());

    let mut via_run = build();
    let actual = via_run.run(a_rising_b_flat_snapshots());

    assert_eq!(actual.equity_curve, expected.equity_curve);
    assert_eq!(actual.fills.len(), expected.fills.len());
    assert_eq!(actual.rejections.len(), expected.rejections.len());
    assert_eq!(actual.initial_equity, expected.initial_equity);
}

/// A child that tries to rest a limit order and records what it was told.
struct Limiter {
    result: std::sync::Arc<std::sync::Mutex<Option<Result<(), WalletError>>>>,
}

impl Strategy for Limiter {
    type Input = Snapshot<&'static str>;
    type Symbol = &'static str;
    fn update(&mut self, _snap: Snapshot<&'static str>) {}
    fn trade(&self, wallet: &mut dyn Wallet<&'static str>) {
        if self.result.lock().unwrap().is_some() {
            return;
        }
        let outcome = wallet
            .set_limit("B", Side::Buy, Size::value_frac(0.5), Reference(60.0))
            .map(|_| ());
        *self.result.lock().unwrap() = Some(outcome);
    }
    fn reset(&mut self) {
        *self.result.lock().unwrap() = None;
    }
}

#[test]
fn a_resting_limit_order_is_refused_inside_a_portfolio() {
    // Documented gap, not an oversight. A netted portfolio has no answer for
    // who owns a resting limit *while it rests*: it isn't in any child's
    // position yet, so it can't be netted, and the account can hold only one
    // per symbol anyway. Refusing is honest; guessing would silently
    // mis-attribute the eventual fill.
    let result: std::sync::Arc<std::sync::Mutex<Option<Result<(), WalletError>>>> =
        Default::default();
    let mut portfolio: Portfolio<&'static str> = PortfolioBuilder::default()
        .with_initial_equity(1_000.0)
        .add(
            "limiter",
            Limiter {
                result: std::sync::Arc::clone(&result),
            },
        )
        .weights(EqualWeight)
        .build();
    let report = portfolio.run(a_rising_b_flat_snapshots());

    assert_eq!(
        *result.lock().unwrap(),
        Some(Err(WalletError::UnsupportedOperation)),
        "a child should be told plainly that limits aren't available here",
    );
    assert!(report.fills.is_empty());
}

#[test]
fn a_child_adjusting_funds_moves_only_its_own_sub_wallet() {
    struct Depositor;
    impl Strategy for Depositor {
        type Input = Snapshot<&'static str>;
        type Symbol = &'static str;
        fn update(&mut self, _snap: Snapshot<&'static str>) {}
        fn trade(&self, wallet: &mut dyn Wallet<&'static str>) {
            // Called once per bar; only the first needs to succeed for the
            // assertion below, but repeating is harmless.
            let _ = wallet.adjust_funds(1.0);
        }
        fn reset(&mut self) {}
    }
    let mut portfolio: Portfolio<&'static str> = PortfolioBuilder::default()
        .with_initial_equity(1_000.0)
        .add("depositor", Depositor)
        .add("idle", submitter(&Default::default(), None))
        .weights(EqualWeight)
        .build();
    let _ = portfolio.run(a_rising_b_flat_snapshots());

    assert!(
        (portfolio.sub_equity(0) - (500.0 + 20.0)).abs() < 1e-9,
        "depositor should have gained 1.0 per bar over 20 bars, got {}",
        portfolio.sub_equity(0),
    );
    assert!(
        (portfolio.sub_equity(1) - 500.0).abs() < 1e-9,
        "the sibling's cash must be untouched, got {}",
        portfolio.sub_equity(1),
    );
}
// ---------------------------------------------------------------------------
// Netting: one account, N ledgers.
//
// The behaviour that only exists because children share a book — the
// sum-to-account identity, internal crossing, and per-child protective legs on
// one net position.
// ---------------------------------------------------------------------------

/// A child that drives one symbol to a fixed unit target on its first trade.
struct HoldUnits {
    symbol: &'static str,
    units: Real,
    done: std::cell::Cell<bool>,
}

impl HoldUnits {
    fn new(symbol: &'static str, units: Real) -> Self {
        Self {
            symbol,
            units,
            done: std::cell::Cell::new(false),
        }
    }
}

impl Strategy for HoldUnits {
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

/// Flat bars on A and B, so nothing moves except what the children do.
fn flat_snapshots(bars: usize) -> Vec<Snapshot<&'static str>> {
    (0..bars)
        .map(|_| {
            let mut s = Snapshot::new();
            s.push(Some("A"), None, Atom::new(flat_bar(100.0)));
            s.push(Some("B"), None, Atom::new(flat_bar(50.0)));
            s
        })
        .collect()
}

#[test]
fn child_ledgers_always_sum_to_the_account() {
    // The identity the whole design rests on, asserted after every bar rather
    // than at the end — a leak would otherwise hide behind a later correction.
    let mut portfolio: Portfolio<&'static str> = PortfolioBuilder::default()
        .with_initial_equity(2_000.0)
        .add("a", SingleAssetStrategy::<&'static str>::buy_and_hold("A"))
        .add("b", SingleAssetStrategy::<&'static str>::buy_and_hold("B"))
        .weights(EqualWeight)
        .build();
    let mut wallet = PaperWallet::new(2_000.0);
    for snap in a_rising_b_flat_snapshots() {
        let _ = backtest::run(&mut portfolio, &mut wallet, [snap]);
        portfolio.assert_books_balance(&wallet);
    }
    // And the parts still add up to the whole at the end.
    let subs = portfolio.sub_equity(0) + portfolio.sub_equity(1);
    assert!(
        (subs - wallet.equity().0).abs() < 1e-6,
        "child equities {subs} != account equity {}",
        wallet.equity().0,
    );
}

#[test]
fn two_children_on_the_same_symbol_send_one_order() {
    // Both children buy A. The account should show a single combined position
    // and a single fill's worth of flow, not two competing ones.
    let mut portfolio: Portfolio<&'static str> = PortfolioBuilder::default()
        .with_initial_equity(2_000.0)
        .add("a1", HoldUnits::new("A", 3.0))
        .add("a2", HoldUnits::new("A", 2.0))
        .weights(EqualWeight)
        .build();
    let mut wallet = PaperWallet::new(2_000.0);
    let report = backtest::run(&mut portfolio, &mut wallet, flat_snapshots(4));

    assert!(
        (wallet.position(&"A").amount - 5.0).abs() < 1e-9,
        "account should hold the combined 5 units, got {}",
        wallet.position(&"A").amount,
    );
    // Same symbol, same side → one netted account order of 5 units (the blotter
    // is account-level now), and each child's ledger holds its own share.
    let bought: Real = report.fills.iter().map(|f| f.order.units).sum();
    assert!((bought - 5.0).abs() < 1e-9, "account fills sum to {bought}");
    assert!((portfolio.sub_position(0, &"A") - 3.0).abs() < 1e-9);
    assert!((portfolio.sub_position(1, &"A") - 2.0).abs() < 1e-9);
    portfolio.assert_books_balance(&wallet);
}

#[test]
fn opposite_sides_cross_internally_and_only_the_imbalance_trades() {
    // A wants +5, B wants -2. Three units reach the market; two cross between
    // the children and never touch it.
    let mut portfolio: Portfolio<&'static str> = PortfolioBuilder::default()
        .with_initial_equity(2_000.0)
        .add("long_a", HoldUnits::new("A", 5.0))
        .add("short_a", HoldUnits::new("A", -2.0))
        .weights(EqualWeight)
        .build();
    let mut wallet = PaperWallet::new(2_000.0);
    let report = backtest::run(&mut portfolio, &mut wallet, flat_snapshots(4));

    assert!(
        (wallet.position(&"A").amount - 3.0).abs() < 1e-9,
        "only the net 3 units should reach the account, got {}",
        wallet.position(&"A").amount,
    );
    // Only the net imbalance reaches the account blotter (the crossed 2 units
    // never touched the market). The per-child positions each child asked for
    // live in its ledger.
    let net_bought: Real = report
        .fills
        .iter()
        .filter(|f| f.order.side == Side::Buy)
        .map(|f| f.order.units)
        .sum();
    assert!((net_bought - 3.0).abs() < 1e-9, "only 3 net units trade, got {net_bought}");
    assert!((portfolio.sub_position(0, &"A") - 5.0).abs() < 1e-9, "long child holds 5");
    assert!((portfolio.sub_position(1, &"A") + 2.0).abs() < 1e-9, "short child holds -2");
    portfolio.assert_books_balance(&wallet);
}

#[test]
fn crossed_flow_pays_no_commission() {
    // The documented cost of netting rather than grossing up: the offsetting
    // part never reached the market, so it is not charged for having done so.
    let costs = TradingCosts::new(
        Box::new(PercentageCommission::new(0.01)),
        Box::new(FixedBpsSpread::new(0.0)),
        Box::new(NoSlippage),
    );
    let run = |long: Real, short: Real| -> fugazi::RunReport<&'static str> {
        let mut portfolio: Portfolio<&'static str> = PortfolioBuilder::default()
            .with_initial_equity(2_000.0)
            .add("long_a", HoldUnits::new("A", long))
            .add("short_a", HoldUnits::new("A", short))
            .weights(EqualWeight)
            .build();
        let mut wallet = PaperWallet::with_costs(2_000.0, costs.clone());
        backtest::run(&mut portfolio, &mut wallet, flat_snapshots(4))
    };

    // 5 long / -2 short: 3 units trade, 2 cross.
    let crossed = run(5.0, -2.0);
    // 3 long / 0: the same 3 units trade with nothing to cross against.
    let plain = run(3.0, 0.0);

    let paid = |r: &fugazi::RunReport<&'static str>| -> Real {
        r.fills.iter().map(|f| f.order.commission).sum()
    };
    assert!(paid(&plain) > 0.0, "sanity: the plain run should pay something");
    assert!(
        (paid(&crossed) - paid(&plain)).abs() < 1e-9,
        "crossing 2 extra units should cost nothing extra: {} vs {}",
        paid(&crossed),
        paid(&plain),
    );
}

/// A child that takes a position and rests a stop at a fixed level.
struct HoldWithStop {
    symbol: &'static str,
    units: Real,
    stop: Real,
    seeded: std::cell::Cell<bool>,
}

impl Strategy for HoldWithStop {
    type Input = Snapshot<&'static str>;
    type Symbol = &'static str;
    fn update(&mut self, _snap: Snapshot<&'static str>) {}
    fn trade(&self, wallet: &mut dyn Wallet<&'static str>) {
        if !self.seeded.get() {
            let _ = wallet.set_position(fugazi::wallet::Units {
                symbol: self.symbol,
                amount: self.units,
            });
            self.seeded.set(true);
            return;
        }
        let _ = wallet.set_stop(
            self.symbol,
            Reference(self.stop),
            Size::position_frac(1.0),
        );
    }
    fn reset(&mut self) {
        self.seeded.set(false);
    }
}

#[test]
fn one_childs_stop_takes_off_only_its_own_share() {
    // The case that made a shared account impossible before protective legs
    // carried a size: two children long the same symbol, one stopped out. The
    // other must still be holding afterwards.
    let mut portfolio: Portfolio<&'static str> = PortfolioBuilder::default()
        .with_initial_equity(4_000.0)
        .add(
            "tight_stop",
            HoldWithStop {
                symbol: "A",
                units: 6.0,
                stop: 95.0,
                seeded: std::cell::Cell::new(false),
            },
        )
        .add(
            "loose_stop",
            HoldWithStop {
                symbol: "A",
                units: 4.0,
                stop: 80.0,
                seeded: std::cell::Cell::new(false),
            },
        )
        .weights(EqualWeight)
        .build();

    // Flat at 100 while both build and rest, then a dip through 95 but not 80.
    let snaps: Vec<Snapshot<&'static str>> = (0..6)
        .map(|bar| {
            let mut s = Snapshot::new();
            let candle = if bar == 4 {
                Candle::new(100.0, 100.0, 90.0, 92.0, 1.0)
            } else {
                flat_bar(100.0)
            };
            s.push(Some("A"), None, Atom::new(candle));
            s
        })
        .collect();
    let mut wallet = PaperWallet::new(4_000.0);
    let _ = backtest::run(&mut portfolio, &mut wallet, snaps);

    // The tight stop fired for its 6 units; the loose child's 4 survive.
    assert!(
        (wallet.position(&"A").amount - 4.0).abs() < 1e-6,
        "expected the loose child's 4 units to remain, got {}",
        wallet.position(&"A").amount,
    );
    portfolio.assert_books_balance(&wallet);
}

#[test]
fn a_child_cannot_spend_past_its_own_slice() {
    // Hard cap. The account is holding $2,000, but each child owns half of it
    // and may not reach into its sibling's share.
    let log: RejectLog = Default::default();
    let mut portfolio: Portfolio<&'static str> = PortfolioBuilder::default()
        .with_initial_equity(2_000.0)
        // 15 units of A at 100 is $1,500 — affordable for the account, but not
        // for this child's $1,000.
        .add("greedy", submitter(&log, Some(("A", Side::Buy, Size::units(15.0)))))
        .add("idle", submitter(&Default::default(), None))
        .weights(EqualWeight)
        .build();
    let mut wallet = PaperWallet::new(2_000.0);
    let _report = backtest::run(&mut portfolio, &mut wallet, flat_snapshots(4));

    // The child hard-cap refusal is a portfolio-ledger concept the account never
    // sees, so it reaches the *child* (via on_reject) but not the run report —
    // the documented consequence of the portfolio no longer owning a wallet.
    assert!(
        log.lock()
            .unwrap()
            .iter()
            .any(|r| r.error == WalletError::InsufficientFunds),
        "the capped child should be told about its own refusal",
    );
    assert!(wallet.position(&"A").amount.abs() < 1e-9, "nothing should have traded");
}

#[test]
fn a_rebalance_moves_notional_cash_without_trading() {
    // On a shared account the cash phase is pure bookkeeping — the balance
    // never moves, only the notional split of it — so a rebalance that needs
    // no position change generates no orders at all.
    use fugazi::indicators::Every;

    let mut portfolio: Portfolio<&'static str> = PortfolioBuilder::default()
        .with_initial_equity(1_000.0)
        .add("idle_a", submitter(&Default::default(), None))
        .add("idle_b", submitter(&Default::default(), None))
        // Start lopsided so the rebalance has work to do.
        .weights(Fixed::new(vec![0.9, 0.1]))
        .rebalance_on(Every::<Snapshot<&'static str>>::new(1))
        .build();
    let mut wallet = PaperWallet::new(1_000.0);
    let report = backtest::run(&mut portfolio, &mut wallet, flat_snapshots(4));

    assert!(
        report.fills.is_empty(),
        "a cash-only rebalance should generate no orders, got {}",
        report.fills.len(),
    );
    // Fixed weights are also the rebalance target, so the split is re-affirmed
    // rather than moved — and the total is untouched either way.
    assert!((wallet.equity().0 - 1_000.0).abs() < 1e-9);
    portfolio.assert_books_balance(&wallet);
}

// ---------------------------------------------------------------------------
// The live-substrate paths.
//
// A `PaperWallet` fills synchronously inside `update`, so three branches of the
// netting layer never run in an ordinary backtest: out-of-band fills via
// `poll_fills`, a submission that fills immediately (`Ack::Filled`), and a
// submitted order that simply hasn't filled yet. A real venue does all three,
// and getting them wrong loses fills silently — so they get a double.
// ---------------------------------------------------------------------------

/// A venue that executes when fed a bar but only *reports* the fill on the next
/// `poll_fills`, the way a real exchange reports through a trades endpoint.
struct AsyncVenue {
    cash: Real,
    positions: std::collections::HashMap<&'static str, Real>,
    marks: std::collections::HashMap<&'static str, Real>,
    queued: Vec<(&'static str, Real)>,
    unreported: Vec<Order<&'static str>>,
    /// When set, submissions fill instantly and report through the `Ack`.
    synchronous: bool,
    next_id: u64,
}

impl AsyncVenue {
    fn new(cash: Real, synchronous: bool) -> Self {
        Self {
            cash,
            positions: Default::default(),
            marks: Default::default(),
            queued: Vec::new(),
            unreported: Vec::new(),
            synchronous,
            next_id: 0,
        }
    }

    fn execute(&mut self, symbol: &'static str, target: Real, price: Real) -> Order<&'static str> {
        let current = self.positions.get(symbol).copied().unwrap_or(0.0);
        let delta = target - current;
        self.positions.insert(symbol, target);
        self.cash -= delta * price;
        let id = OrderId(self.next_id);
        self.next_id += 1;
        Order::new(
            symbol,
            if delta > 0.0 { Side::Buy } else { Side::Sell },
            delta.abs(),
            price,
            OrderKind::Market,
            id,
        )
    }
}

impl Wallet<&'static str> for AsyncVenue {
    fn funds(&self) -> Reference {
        Reference(self.cash)
    }
    fn position(&self, symbol: &&'static str) -> Units<&'static str> {
        Units {
            symbol,
            amount: self.positions.get(symbol).copied().unwrap_or(0.0),
        }
    }
    fn price(&self, symbol: &&'static str) -> Option<Reference> {
        self.marks.get(symbol).map(|&p| Reference(p))
    }
    fn equity(&self) -> Reference {
        let held: Real = self
            .positions
            .iter()
            .map(|(s, &a)| a * self.marks.get(s).copied().unwrap_or(0.0))
            .sum();
        Reference(self.cash + held)
    }
    fn update(&mut self, symbol: &'static str, candle: Candle) -> Vec<Order<&'static str>> {
        self.marks.insert(symbol, candle.close);
        let due: Vec<(&'static str, Real)> =
            self.queued.iter().copied().filter(|(s, _)| *s == symbol).collect();
        self.queued.retain(|(s, _)| *s != symbol);
        for (sym, target) in due {
            let order = self.execute(sym, target, candle.open);
            // Executed, but deliberately not returned here — the caller learns
            // about it through `poll_fills`, like a real venue.
            self.unreported.push(order);
        }
        Vec::new()
    }
    fn poll_fills(&mut self) -> Vec<Order<&'static str>> {
        std::mem::take(&mut self.unreported)
    }
    fn set_position(
        &mut self,
        target: Units<&'static str>,
    ) -> Result<Ack<&'static str>, WalletError> {
        if self.synchronous {
            let price = self.price(&target.symbol).ok_or(WalletError::UnknownPrice)?.0;
            let order = self.execute(target.symbol, target.amount, price);
            return Ok(Ack::Filled(order));
        }
        self.queued.push((target.symbol, target.amount));
        let id = OrderId(self.next_id);
        self.next_id += 1;
        Ok(Ack::Working(id))
    }
    fn set(
        &mut self,
        symbol: &'static str,
        side: Side,
        size: Size,
    ) -> Result<Ack<&'static str>, WalletError> {
        let price = self.price(&symbol).ok_or(WalletError::UnknownPrice)?.0;
        let position = self.position(&symbol).amount;
        let magnitude = size.resolve(price, position, self.cash, self.equity().0);
        self.set_position(Units {
            symbol,
            amount: side.sign() * magnitude,
        })
    }
    fn set_stop(
        &mut self,
        _symbol: &'static str,
        _trigger: Reference,
        _size: Size,
    ) -> Result<Ack<&'static str>, WalletError> {
        Ok(Ack::Working(OrderId(u64::MAX)))
    }
    fn set_take_profit(
        &mut self,
        _symbol: &'static str,
        _trigger: Reference,
        _size: Size,
    ) -> Result<Ack<&'static str>, WalletError> {
        Ok(Ack::Working(OrderId(u64::MAX)))
    }
    fn cancel_protective(&mut self, _symbol: &&'static str) -> Result<(), WalletError> {
        Ok(())
    }
    fn positions(&self) -> Vec<Units<&'static str>> {
        self.positions
            .iter()
            .map(|(&symbol, &amount)| Units { symbol, amount })
            .collect()
    }
}

#[test]
fn fills_reported_out_of_band_still_reach_the_ledgers() {
    // The branch a `PaperWallet` can never exercise. Before `poll_fills` was
    // forwarded, a venue that reports through a trades endpoint would move the
    // account while every child's ledger stayed empty — the account and the
    // books would silently disagree forever.
    let mut portfolio: Portfolio<&'static str> = PortfolioBuilder::default()
        .with_initial_equity(2_000.0)
        .add("a", HoldUnits::new("A", 4.0))
        .add("b", HoldUnits::new("B", 6.0))
        .weights(EqualWeight)
        .build();
    // Drive the portfolio directly against the async venue — the portfolio now
    // trades whatever wallet it is handed, so this exercises the real driver's
    // `poll_fills` loop routing out-of-band fills into `Portfolio::on_fill`.
    let mut venue = AsyncVenue::new(2_000.0, false);
    let report = backtest::run(&mut portfolio, &mut venue, flat_snapshots(5));

    assert!(!report.fills.is_empty(), "the out-of-band fills should surface");
    assert!(
        (venue.position(&"A").amount - 4.0).abs() < 1e-9,
        "account A = {}",
        venue.position(&"A").amount,
    );
    assert!((venue.position(&"B").amount - 6.0).abs() < 1e-9);
    // The point of the test: the books tracked the account through a fill
    // stream that never came back from `update`.
    portfolio.assert_books_balance(&venue);
}

#[test]
fn a_venue_that_fills_on_submission_still_reaches_the_ledgers() {
    // `Ack::Filled` — a venue that executes synchronously. There is no later
    // update-stream entry for such a fill, so attributing it at submission is
    // the only chance to move a ledger.
    let mut portfolio: Portfolio<&'static str> = PortfolioBuilder::default()
        .with_initial_equity(2_000.0)
        .add("a", HoldUnits::new("A", 4.0))
        .weights(EqualWeight)
        .build();
    let mut venue = AsyncVenue::new(2_000.0, true);
    let _ = backtest::run(&mut portfolio, &mut venue, flat_snapshots(5));

    assert!(
        (venue.position(&"A").amount - 4.0).abs() < 1e-9,
        "account A = {}",
        venue.position(&"A").amount,
    );
    assert!(
        (portfolio.sub_equity(0) - 2_000.0).abs() < 1e-6,
        "flat prices, so the child's equity should be unchanged: {}",
        portfolio.sub_equity(0),
    );
    portfolio.assert_books_balance(&venue);
}

// ---------------------------------------------------------------------------
// A portfolio is an ordinary strategy over the wallet it is handed.
// ---------------------------------------------------------------------------

#[test]
fn portfolio_trades_the_passed_wallet() {
    // The headline of the refactor: no `wallet_view`, no owned substrate — the
    // portfolio nets its children onto whatever wallet `backtest::run` gives it.
    let mut portfolio: Portfolio<&'static str> = PortfolioBuilder::default()
        .with_initial_equity(2_000.0)
        .add("a", SingleAssetStrategy::<&'static str>::buy_and_hold("A"))
        .add("b", SingleAssetStrategy::<&'static str>::buy_and_hold("B"))
        .weights(EqualWeight)
        .build();
    let mut wallet = PaperWallet::new(2_000.0);
    let report = backtest::run(&mut portfolio, &mut wallet, a_rising_b_flat_snapshots());

    assert!(wallet.position(&"A").amount > 0.0, "child A opened on the account");
    assert!(wallet.position(&"B").amount > 0.0, "child B opened on the account");
    assert!(
        (wallet.equity().0 - *report.equity_curve.last().unwrap()).abs() < 1e-6,
        "the reported curve is the account's own equity",
    );
    portfolio.assert_books_balance(&wallet);
}

#[test]
fn internal_cross_books_at_open_with_no_wallet_fill() {
    // A fully-crossed symbol (net 0) submits no order, so nothing reaches the
    // account or the blotter — but both children's ledgers still book at the
    // bar open, commission-free. Locks in the cross-booking path in `update`.
    let mut portfolio: Portfolio<&'static str> = PortfolioBuilder::default()
        .with_initial_equity(2_000.0)
        .add("long_a", HoldUnits::new("A", 5.0))
        .add("short_a", HoldUnits::new("A", -5.0))
        .weights(EqualWeight)
        .build();
    let mut wallet = PaperWallet::new(2_000.0);
    let report = backtest::run(&mut portfolio, &mut wallet, flat_snapshots(4));

    assert!(wallet.position(&"A").amount.abs() < 1e-9, "net 0 reaches the account");
    assert!(
        report.fills.iter().all(|f| f.order.symbol != "A"),
        "a fully-crossed symbol never appears in the blotter",
    );
    assert!((portfolio.sub_position(0, &"A") - 5.0).abs() < 1e-9, "long child holds 5");
    assert!((portfolio.sub_position(1, &"A") + 5.0).abs() < 1e-9, "short child holds -5");
    portfolio.assert_books_balance(&wallet);
}

#[test]
fn a_sleeve_lets_a_portfolio_coexist_with_external_positions() {
    use fugazi::wallet::{SleeveWallet, external_baseline};
    // The account already holds the user's own 3 units of C. Run the portfolio
    // over a sleeve of it: the portfolio sees only its own book, trading A while
    // the external C position is left untouched. Same decorator the direct shapes
    // use, now over a portfolio's account.
    let mut account = PaperWallet::new(2_000.0);
    account.update("C", flat_bar(100.0));
    let _ = account.set_position(fugazi::wallet::Units { symbol: "C", amount: 3.0 });
    account.update("C", flat_bar(100.0)); // fills at open 100 → holds 3 C
    assert!((account.position(&"C").amount - 3.0).abs() < 1e-9);

    let baseline = external_baseline(&account);
    let mut view = SleeveWallet::new(account, baseline);
    let seed = view.equity().0; // own equity: cash only, C excluded

    let mut portfolio: Portfolio<&'static str> = PortfolioBuilder::default()
        .with_initial_equity(seed)
        .add("a", SingleAssetStrategy::<&'static str>::buy_and_hold("A"))
        .weights(EqualWeight)
        .build();
    let _ = backtest::run(&mut portfolio, &mut view, a_rising_b_flat_snapshots());

    let account = view.into_inner();
    assert!(
        (account.position(&"C").amount - 3.0).abs() < 1e-9,
        "the external C position must be left untouched",
    );
    assert!(account.position(&"A").amount > 0.0, "the portfolio opened its own A position");
}

/// A child trades a `LedgerWallet`, which holds no handle on the account — so
/// `can_short()` on that handle has to carry the account's answer across, or a
/// child would size a short the account can never hold.
#[test]
fn a_child_reads_the_accounts_can_short_through_its_ledger_handle() {
    use std::sync::{Arc, Mutex};

    /// Records what its wallet handle reported, each bar it trades.
    struct AsksCanShort {
        seen: Arc<Mutex<Option<bool>>>,
    }
    impl Strategy for AsksCanShort {
        type Input = Snapshot<&'static str>;
        type Symbol = &'static str;
        fn update(&mut self, _snap: Snapshot<&'static str>) {}
        fn trade(&self, wallet: &mut dyn Wallet<&'static str>) {
            *self.seen.lock().unwrap() = Some(wallet.can_short());
        }
        fn reset(&mut self) {
            *self.seen.lock().unwrap() = None;
        }
    }

    /// A spot-shaped account: it cannot hold a negative position and says so.
    struct SpotAccount(PaperWallet<&'static str>);
    impl Wallet<&'static str> for SpotAccount {
        fn can_short(&self) -> bool {
            false
        }
        fn funds(&self) -> Reference {
            self.0.funds()
        }
        fn position(&self, s: &&'static str) -> fugazi::wallet::Units<&'static str> {
            self.0.position(s)
        }
        fn positions(&self) -> Vec<fugazi::wallet::Units<&'static str>> {
            self.0.positions()
        }
        fn price(&self, s: &&'static str) -> Option<Reference> {
            self.0.price(s)
        }
        fn equity(&self) -> Reference {
            self.0.equity()
        }
        fn update(&mut self, s: &'static str, c: Candle) -> Vec<Order<&'static str>> {
            self.0.update(s, c)
        }
        fn set_position(
            &mut self,
            t: fugazi::wallet::Units<&'static str>,
        ) -> Result<fugazi::wallet::Ack<&'static str>, fugazi::wallet::WalletError> {
            self.0.set_position(fugazi::wallet::Units {
                symbol: t.symbol,
                amount: t.amount.max(0.0),
            })
        }
        fn set_stop(
            &mut self,
            s: &'static str,
            t: Reference,
            size: fugazi::wallet::Size,
        ) -> Result<fugazi::wallet::Ack<&'static str>, fugazi::wallet::WalletError> {
            self.0.set_stop(s, t, size)
        }
        fn set_take_profit(
            &mut self,
            s: &'static str,
            t: Reference,
            size: fugazi::wallet::Size,
        ) -> Result<fugazi::wallet::Ack<&'static str>, fugazi::wallet::WalletError> {
            self.0.set_take_profit(s, t, size)
        }
        fn cancel_protective(
            &mut self,
            s: &&'static str,
        ) -> Result<(), fugazi::wallet::WalletError> {
            self.0.cancel_protective(s)
        }
    }

    let seen = Arc::new(Mutex::new(None));
    let mut portfolio: Portfolio<&'static str> = PortfolioBuilder::default()
        .with_initial_equity(1_000.0)
        .add("asks", AsksCanShort { seen: Arc::clone(&seen) })
        .weights(EqualWeight)
        .build();
    let mut spot = SpotAccount(PaperWallet::new(1_000.0));
    let _ = backtest::run(&mut portfolio, &mut spot, a_rising_b_flat_snapshots());
    assert_eq!(
        *seen.lock().unwrap(),
        Some(false),
        "the spot account's answer reaches the child",
    );

    // Over an ordinary paper account the same child sees the permissive answer.
    let seen = Arc::new(Mutex::new(None));
    let mut portfolio: Portfolio<&'static str> = PortfolioBuilder::default()
        .with_initial_equity(1_000.0)
        .add("asks", AsksCanShort { seen: Arc::clone(&seen) })
        .weights(EqualWeight)
        .build();
    let mut paper = PaperWallet::new(1_000.0);
    let _ = backtest::run(&mut portfolio, &mut paper, a_rising_b_flat_snapshots());
    assert_eq!(*seen.lock().unwrap(), Some(true));
}
