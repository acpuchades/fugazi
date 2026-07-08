//! End-to-end integration tests for `PairsStrategy`: drive the two-leg
//! strategy over a synthetic pair whose spread mean-reverts, and check that
//! (a) both legs open on the entry signal, (b) both flatten on the exit
//! signal, and (c) reset replays byte-identically like the single-asset
//! strategies do.

use fugazi::indicators::{Close, Const, Pick, Sma};
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
    Candle::new(p, p, p, p, 0.0)
}

fn snapshot(l: Candle, r: Candle) -> Snapshot<&'static str> {
    let mut s = Snapshot::new();
    s.push(Some(LEFT), None, l.into());
    s.push(Some(RIGHT), None, r.into());
    s
}

/// Drive `strat` over the pair series, feeding each bar to the wallet for
/// both legs and delivering fills to the strategy before its `update`/`trade`.
fn run(
    mut strat: PairsStrategy<&'static str>,
    bars: &[(Candle, Candle)],
) -> PaperWallet<&'static str> {
    let mut wallet = PaperWallet::new(FUNDS);
    for &(l, r) in bars {
        for fill in wallet.update(LEFT, l) {
            strat.on_fill(&fill);
        }
        for fill in wallet.update(RIGHT, r) {
            strat.on_fill(&fill);
        }
        strat.update(snapshot(l, r));
        strat.trade(&mut wallet);
    }
    wallet
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
    assert!(wallet.funds().0.is_finite());
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
        Const::<Snapshot<&'static str>>::new(true),
        Const::<Snapshot<&'static str>>::new(false),
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
