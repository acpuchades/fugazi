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
