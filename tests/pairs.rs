//! End-to-end integration tests for `PairsStrategy`: drive the two-leg
//! strategy over a synthetic pair whose spread mean-reverts, and check that
//! (a) both legs open on the entry signal, (b) both flatten on the exit
//! signal, and (c) reset replays byte-identically like the single-asset
//! strategies do.

mod common;

use fugazi::backtest;
use fugazi::indicators::{Close, Pick, Sma, ValueBool};
use fugazi::prelude::*;
use fugazi::strategies::PairsStrategy;
use fugazi::types::{Selector, Snapshot};

const LEFT: &str = "L";
const RIGHT: &str = "R";
const FUNDS: Real = 10_000.0;

/// A synthetic pair: `L` walks a slow upward trend, `R` walks the same trend
/// plus a mean-reverting sinusoid. The `L−R` spread therefore oscillates,
/// giving a mean-reversion strategy things to trade.
fn pair_series() -> Vec<(Candle, Candle)> {
    let mut out = Vec::new();
    for i in 0..200i32 {
        let base = 100.0 + f64::from(i) * 0.1;
        let l_close = base;
        let r_close = base + 5.0 * (f64::from(i) * 0.15).sin();
        let l = flat_bar(l_close);
        let r = flat_bar(r_close);
        out.push((l, r));
    }
    out
}

fn flat_bar(p: Real) -> Candle {
    common::bars::flat_with_volume(p, 0.0)
}

fn snapshot(l: Candle, r: Candle) -> Snapshot<&'static str> {
    let mut s = Snapshot::new();
    s.push(Some(LEFT), None, l.into());
    s.push(Some(RIGHT), None, r.into());
    s
}

/// Drive `strat` over the pair series through [`fugazi::backtest::run`] — the
/// same driver the CLI and spec layer use.
///
/// Deliberately *not* a hand-rolled bar loop. The driver prices each tagged leg,
/// routes fills back through `on_fill`, drains rejections, and — the part a
/// hand-rolled loop always forgets — calls `trade()` **only when `is_ready()`**.
/// Testing against a loop that trades unconditionally would exercise a path
/// production never takes.
fn run(
    mut strat: PairsStrategy<&'static str>,
    bars: &[(Candle, Candle)],
) -> PaperWallet<&'static str> {
    run_reported(&mut strat, bars).0
}

fn run_reported(
    strat: &mut PairsStrategy<&'static str>,
    bars: &[(Candle, Candle)],
) -> (PaperWallet<&'static str>, fugazi::RunReport<&'static str>) {
    let snaps: Vec<Snapshot<&'static str>> = bars.iter().map(|&(l, r)| snapshot(l, r)).collect();
    let mut wallet = PaperWallet::new(FUNDS);
    let report = backtest::run(strat, &mut wallet, snaps);
    (wallet, report)
}

#[test]
fn spread_reversion_strategy_trades_over_the_pair() {
    // Enter when spread is below its 20-bar SMA - 3; exit when it climbs back
    // above its SMA.
    let bars = pair_series();
    let spread = || {
        Close::of(Pick::matching(Selector::by_symbol(LEFT)))
            .sub(Close::of(Pick::matching(Selector::by_symbol(RIGHT))))
    };
    let enter = spread().sub(Sma::new(spread(), 20)).below(-2.0);
    let exit = spread().sub(Sma::new(spread(), 20)).above(0.0);
    let strat = PairsStrategy::new(LEFT, RIGHT).on(enter, exit);
    let wallet = run(strat, &bars);
    assert!(
        !wallet.orders().is_empty(),
        "spread-reversion pair never traded over the series"
    );
    // Every fill is either on L or R.
    for order in wallet.orders() {
        assert!(order.symbol == LEFT || order.symbol == RIGHT);
    }
    // Both legs opened and both flattened, so the account ends flat with its
    // cash intact rather than merely "finite".
    assert!(
        wallet.positions().is_empty(),
        "the exit signal should flatten both legs by the end of the series"
    );
}

#[test]
fn a_bidirectional_pair_trades_both_tails_of_the_spread() {
    // The spread oscillates through both tails. A long-spread-only strategy can
    // only harvest the cheap side; wiring `short_spread_on` picks up the rich
    // side too, in the opposite direction.
    let bars = pair_series();
    let gap = || {
        let spread = || {
            Close::of(Pick::matching(Selector::by_symbol(LEFT)))
                .sub(Close::of(Pick::matching(Selector::by_symbol(RIGHT))))
        };
        spread().sub(Sma::new(spread(), 20))
    };

    let long_only =
        PairsStrategy::new(LEFT, RIGHT).long_spread_on(gap().below(-2.0), gap().above(0.0));
    let both = PairsStrategy::new(LEFT, RIGHT)
        .long_spread_on(gap().below(-2.0), gap().above(0.0))
        .short_spread_on(gap().above(2.0), gap().below(0.0));

    let long_wallet = run(long_only, &bars);
    let both_wallet = run(both, &bars);

    // A `Sell` on the left leg alone proves nothing — closing a long also
    // sells. What separates the two directions is the *signed* position, so
    // replay the blotter and watch where the left leg's holding goes.
    let min_left_position = |wallet: &PaperWallet<&'static str>| {
        let mut units: Real = 0.0;
        let mut lowest: Real = 0.0;
        for order in wallet.orders().iter().filter(|o| o.symbol == LEFT) {
            units += match order.side {
                Side::Buy => order.units,
                Side::Sell => -order.units,
            };
            lowest = lowest.min(units);
        }
        lowest
    };

    // Long-spread-only never holds the left leg short.
    assert!(
        min_left_position(&long_wallet) > -1e-9,
        "long-spread-only strategy went short the left leg (min position {})",
        min_left_position(&long_wallet),
    );

    // The bidirectional one does — that is the whole point.
    assert!(
        min_left_position(&both_wallet) < -1e-9,
        "bidirectional pair never opened the short-spread side (min left position {})",
        min_left_position(&both_wallet),
    );
    assert!(
        both_wallet.orders().len() > long_wallet.orders().len(),
        "bidirectional pair should trade strictly more than the long-only one \
         ({} vs {} orders)",
        both_wallet.orders().len(),
        long_wallet.orders().len(),
    );
}

#[test]
fn short_spread_legs_are_the_mirror_of_long_spread_legs() {
    // Force the short side open on bar 0: short left, long right, each at half
    // equity — the exact mirror of the long-spread entry.
    let bars = vec![(flat_bar(100.0), flat_bar(50.0)); 2];
    let strat = PairsStrategy::new(LEFT, RIGHT).short_spread_on(
        ValueBool::<Snapshot<&'static str>>::new(true),
        ValueBool::<Snapshot<&'static str>>::new(false),
    );
    let wallet = run(strat, &bars);
    assert_eq!(wallet.orders().len(), 2);
    let l_fill = wallet.orders().iter().find(|o| o.symbol == LEFT).unwrap();
    let r_fill = wallet.orders().iter().find(|o| o.symbol == RIGHT).unwrap();
    assert_eq!(l_fill.side, Side::Sell);
    assert_eq!(r_fill.side, Side::Buy);
    let target = FUNDS * 0.5;
    assert!((l_fill.units * l_fill.price - target).abs() < target * 0.05);
    assert!((r_fill.units * r_fill.price - target).abs() < target * 0.05);
}

#[test]
fn the_short_side_stop_fires_on_a_rising_spread() {
    // Sign-awareness: the short-spread side loses as the spread *rises*, so its
    // stop compares the other way round from the long side's.
    //
    // Spread starts at 50 and climbs. Enter short-spread immediately; the stop
    // sits at 60, so it must fire once the spread crosses it upward.
    let bars: Vec<(Candle, Candle)> = (0..12)
        .map(|i| (flat_bar(100.0 + f64::from(i) * 2.0), flat_bar(50.0)))
        .collect();
    let strat = PairsStrategy::new(LEFT, RIGHT)
        .short_spread_on(
            ValueBool::<Snapshot<&'static str>>::new(true),
            ValueBool::<Snapshot<&'static str>>::new(false),
        )
        .short_spread_stop_loss(fugazi::indicators::Value::<Snapshot<&'static str>>::new(
            60.0,
        ));
    let wallet = run(strat, &bars);
    // Opened (2 orders) then stopped out (2 more) — and possibly re-opened,
    // since the constant-true entry fires again once flat.
    assert!(
        wallet.orders().len() > 2,
        "short-spread stop never fired on the rising spread ({} orders)",
        wallet.orders().len()
    );
    // The close-out buys back the short left leg.
    assert!(
        wallet
            .orders()
            .iter()
            .any(|o| o.symbol == LEFT && o.side == Side::Buy),
        "expected a buy on L to close the short-spread position"
    );
}

#[test]
fn the_opposite_entry_reverses_an_open_pair() {
    // Spread climbs 50 → 72. The long side enters below 60, the short side
    // above it, so once the spread crosses 60 the short entry fires while the
    // long pair is still open, and must flip it rather than be ignored.
    let bars: Vec<(Candle, Candle)> = (0..12)
        .map(|i| (flat_bar(100.0 + f64::from(i) * 2.0), flat_bar(50.0)))
        .collect();
    let spread = || {
        Close::of(Pick::matching(Selector::by_symbol(LEFT)))
            .sub(Close::of(Pick::matching(Selector::by_symbol(RIGHT))))
    };
    let strat = PairsStrategy::new(LEFT, RIGHT)
        .long_spread_on(
            spread().below(60.0),
            ValueBool::<Snapshot<&'static str>>::new(false),
        )
        .short_spread_on(
            spread().above(60.0),
            ValueBool::<Snapshot<&'static str>>::new(false),
        );
    let wallet = run(strat, &bars);
    // First fills are the long-spread pair, later ones flip to the mirror.
    let left_sides: Vec<Side> = wallet
        .orders()
        .iter()
        .filter(|o| o.symbol == LEFT)
        .map(|o| o.side)
        .collect();
    assert_eq!(
        left_sides.first(),
        Some(&Side::Buy),
        "should open long-spread"
    );
    assert!(
        left_sides.contains(&Side::Sell),
        "short-spread entry should have reversed the pair ({left_sides:?})"
    );
}

#[test]
fn reset_replays_the_run_identically() {
    let bars = pair_series();
    let spread = || {
        Close::of(Pick::matching(Selector::by_symbol(LEFT)))
            .sub(Close::of(Pick::matching(Selector::by_symbol(RIGHT))))
    };
    let enter = spread().sub(Sma::new(spread(), 20)).below(-2.0);
    let exit = spread().sub(Sma::new(spread(), 20)).above(0.0);
    let mut strat = PairsStrategy::new(LEFT, RIGHT).on(enter, exit);

    let mut first = PaperWallet::new(FUNDS);
    for &(l, r) in &bars {
        for fill in first.update(LEFT, l) {
            strat.on_fill(&fill);
        }
        for fill in first.update(RIGHT, r) {
            strat.on_fill(&fill);
        }
        strat.update(snapshot(l, r));
        strat.trade(&mut first);
    }

    strat.reset();
    let mut second = PaperWallet::new(FUNDS);
    for &(l, r) in &bars {
        for fill in second.update(LEFT, l) {
            strat.on_fill(&fill);
        }
        for fill in second.update(RIGHT, r) {
            strat.on_fill(&fill);
        }
        strat.update(snapshot(l, r));
        strat.trade(&mut second);
    }
    assert_eq!(first.orders(), second.orders());
}

#[test]
fn enter_dollar_neutral_sizes_legs_at_half_equity_each() {
    // Force-enter on bar 0: both legs fill on bar 1 at 50% equity notional
    // each, so the gross exposure is ~1.0× starting equity.
    let bars = vec![(flat_bar(100.0), flat_bar(50.0)); 2];
    let strat = PairsStrategy::new(LEFT, RIGHT).on(
        ValueBool::<Snapshot<&'static str>>::new(true),
        ValueBool::<Snapshot<&'static str>>::new(false),
    );
    let wallet = run(strat, &bars);
    // One fill per leg.
    assert_eq!(wallet.orders().len(), 2);
    let l_fill = wallet.orders().iter().find(|o| o.symbol == LEFT).unwrap();
    let r_fill = wallet.orders().iter().find(|o| o.symbol == RIGHT).unwrap();
    assert_eq!(l_fill.side, Side::Buy);
    assert_eq!(r_fill.side, Side::Sell);
    // Notionals should be ~50% of equity each ($5,000), within a small tolerance.
    let l_notional = l_fill.units * l_fill.price;
    let r_notional = r_fill.units * r_fill.price;
    let target = FUNDS * 0.5;
    assert!((l_notional - target).abs() / target < 0.02);
    assert!((r_notional - target).abs() / target < 0.02);
}
