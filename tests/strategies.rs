//! Behavioural tests for the built-in strategy catalogue: every strategy is run
//! over a synthetic price path that both trends (up then down) and oscillates —
//! so trend, breakout, mean-reversion, momentum, volume and composite
//! strategies all find something to trade.
//!
//! Everything here goes through [`fugazi::backtest::run`], the same driver the
//! CLI and the spec layer use. That matters: the driver calls `trade()` **only
//! when `is_ready()`**, and it routes fills and rejections back into the
//! strategy in a defined order. A hand-rolled `for` loop that calls
//! `strat.trade(&mut wallet)` unconditionally — which this file used to do —
//! exercises a path production never takes, so a readiness regression could
//! pass here and still ship.

mod common;

use fugazi::backtest;
use fugazi::prelude::*;
use fugazi::strategies::composite::{adx_trend_filter, keltner_breakout, rsi_pullback};
use fugazi::strategies::mean_reversion::{
    ZScoreReversion, bollinger_reversion, mfi_reversal, rsi_reversal, stoch_rsi_reversal,
    stochastic_reversal,
};
use fugazi::strategies::momentum::{momentum_roc, rsi_midline};
use fugazi::strategies::trend::{
    bollinger_breakout, donchian_breakout, ma_crossover, macd_crossover, macd_zero_cross, triple_ma,
};
use fugazi::strategies::volume::{chaikin_ad_trend, obv_trend, vwap_reversion};
use fugazi::types::Snapshot;

const SYMBOL: &str = "X";
const FUNDS: Real = 10_000.0;

type Catalogue = dyn Strategy<Input = Snapshot<&'static str>, Symbol = &'static str>;

/// Builds a fresh instance of one catalogue entry. Boxed rather than generic so
/// every entry lives in one list, and a *factory* rather than a value because
/// the assertions need two independent instances (a traded run and an untraded
/// readiness probe) and the catalogue's strategies are not `Clone`.
type Factory = Box<dyn Fn() -> Box<Catalogue>>;

/// Two full rise-then-fall cycles with a steady oscillation on top — rich
/// enough to exercise every strategy family.
///
/// **Two** cycles, not one, and 400 bars rather than 200, because the driver
/// honours `is_ready()`: the slowest catalogue entries stack recursive
/// smoothers (`macd_zero_cross` is three EMAs deep) and only settle well past
/// bar 200. A single-cycle path let their one trend reversal pass by while they
/// were still unstable, so they never traded at all — which the old
/// un-gated driver hid.
fn series() -> Vec<Candle> {
    const CYCLE: i32 = 100;
    let mut candles = Vec::new();
    let mut prev_close: Real = 100.0;
    for i in 0..4 * CYCLE {
        let phase = i % (2 * CYCLE);
        let trend = if phase < CYCLE {
            100.0 + f64::from(phase) * 0.8
        } else {
            180.0 - f64::from(phase - CYCLE) * 0.8
        };
        let close = trend + 10.0 * (f64::from(i) * 0.25).sin();
        let open = prev_close;
        let high = open.max(close) + 0.75;
        let low = open.min(close) - 0.75;
        // Volume scales with the size of the move (regardless of direction), so
        // money-flow indicators reach their extremes on the steep swings while
        // OBV/AD still read trend from the sign of each bar.
        let volume = 1_000.0 + 200.0 * (close - open).abs();
        candles.push(Candle::new(open, high, low, close, volume));
        prev_close = close;
    }
    candles
}

/// The candles as the one-symbol tagged snapshots `backtest::run` prices from.
/// (An *untagged* snapshot is visible to the strategy but skipped for wallet
/// pricing, so the symbol tag is what makes the run trade at all.)
fn snapshots(candles: &[Candle]) -> Vec<Snapshot<&'static str>> {
    candles
        .iter()
        .map(|&c| Snapshot::single(SYMBOL, c.into()))
        .collect()
}

/// Drive `strat` over `candles` through the production driver.
fn run<S>(strat: &mut S, candles: &[Candle]) -> (PaperWallet<&'static str>, fugazi::RunReport<&'static str>)
where
    S: Strategy<Input = Snapshot<&'static str>, Symbol = &'static str> + ?Sized,
{
    let mut wallet = PaperWallet::new(FUNDS);
    let report = backtest::run(strat, &mut wallet, snapshots(candles));
    (wallet, report)
}

/// The first bar on which `strat` reports itself ready, found by advancing it
/// over the same stream **without** trading.
fn first_ready_bar<S>(strat: &mut S, candles: &[Candle]) -> Option<usize>
where
    S: Strategy<Input = Snapshot<&'static str>, Symbol = &'static str> + ?Sized,
{
    for (bar, snap) in snapshots(candles).into_iter().enumerate() {
        strat.update(snap);
        if strat.is_ready() {
            return Some(bar);
        }
    }
    None
}

/// The catalogue-wide contract, asserted for one strategy.
///
/// `make` is called twice — once for the traded run, once for the untraded
/// readiness probe — because the catalogue's strategies are not `Clone` and the
/// probe must not be contaminated by fills.
#[track_caller]
fn assert_catalogue_contract(name: &str, make: impl Fn() -> Box<Catalogue>, candles: &[Candle]) {
    let (wallet, report) = run(&mut *make(), candles);

    assert!(!wallet.orders().is_empty(), "{name} never traded");
    assert_eq!(
        report.equity_curve.len(),
        candles.len(),
        "{name}: one equity reading per bar"
    );
    assert!(
        report.equity_curve.iter().all(|e| e.is_finite()),
        "{name} produced a non-finite equity reading"
    );
    assert!(
        report.rejections.is_empty(),
        "{name} had orders refused, so its curve describes a different strategy: {:?}",
        report.rejections
    );

    // The safe-by-default readiness gate: nothing may fill before the strategy
    // declares itself ready. This is the invariant the whole `stable_period`
    // machinery exists to enforce, and driving through `backtest::run` is what
    // makes it observable at all.
    let ready = first_ready_bar(&mut *make(), candles)
        .unwrap_or_else(|| panic!("{name} never became ready over {} bars", candles.len()));
    let first_fill = report.fills.first().expect("asserted non-empty above").bar;
    assert!(
        first_fill >= ready,
        "{name} filled on bar {first_fill} but only became ready on bar {ready}"
    );

    // A backtest never fills on the signal's own bar — the wallet queues market
    // moves and flushes them at the *next* bar's open.
    assert!(first_fill > 0, "{name} filled on bar 0");
}

/// Every catalogue entry, as a fresh-instance factory. A new strategy is one
/// line here and inherits every assertion in [`assert_catalogue_contract`].
fn catalogue() -> Vec<(&'static str, Factory)> {
    macro_rules! entry {
        ($name:literal, $make:expr) => {
            ($name, Box::new(|| Box::new($make) as Box<Catalogue>) as Factory)
        };
    }
    vec![
        // Trend-following.
        entry!("ma_crossover", ma_crossover(SYMBOL, 5, 20)),
        entry!("macd_crossover", macd_crossover(SYMBOL, 12, 26, 9)),
        entry!("macd_zero_cross", macd_zero_cross(SYMBOL, 12, 26, 9)),
        entry!("donchian_breakout", donchian_breakout(SYMBOL, 20)),
        entry!("triple_ma", triple_ma(SYMBOL, 5, 10, 20)),
        entry!("bollinger_breakout", bollinger_breakout(SYMBOL, 20, 2.0)),
        // Mean-reversion.
        entry!("rsi_reversal", rsi_reversal(SYMBOL, 14, 30.0, 50.0)),
        entry!("bollinger_reversion", bollinger_reversion(SYMBOL, 20, 2.0)),
        entry!("stochastic_reversal", stochastic_reversal(SYMBOL, 14, 0.2, 0.8)),
        entry!("stoch_rsi_reversal", stoch_rsi_reversal(SYMBOL, 14, 14, 0.2, 0.8)),
        entry!("mfi_reversal", mfi_reversal(SYMBOL, 14, 20.0, 80.0)),
        entry!("ZScoreReversion", ZScoreReversion::new(SYMBOL, 20, 1.0)),
        // Momentum.
        entry!("momentum_roc", momentum_roc(SYMBOL, 10)),
        entry!("rsi_midline", rsi_midline(SYMBOL, 14)),
        // Volume / flow.
        entry!("obv_trend", obv_trend(SYMBOL, 20)),
        entry!("vwap_reversion", vwap_reversion(SYMBOL, 20)),
        entry!("chaikin_ad_trend", chaikin_ad_trend(SYMBOL, 20)),
        // Composite.
        entry!("adx_trend_filter", adx_trend_filter(SYMBOL, 5, 20, 14, 10.0)),
        // A Connors-style short-period RSI: a 14-period RSI rarely pulls back to
        // oversold mid-uptrend, but RSI(2) dips hard on any down-bar.
        entry!("rsi_pullback", rsi_pullback(SYMBOL, 2, 20, 15.0, 60.0)),
        entry!("keltner_breakout", keltner_breakout(SYMBOL, 20, 10, 2.0)),
    ]
}

#[test]
fn every_strategy_trades_over_the_path() {
    let c = series();
    for (name, make) in catalogue() {
        assert_catalogue_contract(name, make, &c);
    }
}

/// The catalogue list above must not silently shrink. Pinning the count is what
/// turns "someone deleted an entry while refactoring" into a failure rather
/// than a quietly narrower sweep.
#[test]
fn the_catalogue_covers_every_built_in_strategy() {
    assert_eq!(
        catalogue().len(),
        20,
        "add the new strategy to `catalogue()` (and bump this count)"
    );
}

#[test]
fn ma_crossover_goes_long_then_short() {
    // Decline first so the MAs warm up with the fast below the slow, then a rise
    // (a genuine golden cross → Buy) and a fall (a death cross → reverse to Sell).
    // The opening decline matters: an edge only registers once both MAs are warm,
    // so the cross must happen after warm-up rather than coincide with it.
    let mut prices: Vec<Real> = (0..10).map(|i| 110.0 - f64::from(i) * 2.0).collect();
    prices.extend((1..=15).map(|i| 92.0 + f64::from(i) * 2.0));
    prices.extend((1..=15).map(|i| 120.0 - f64::from(i) * 2.0));
    let candles: Vec<Candle> = prices
        .iter()
        .map(|&p| common::bars::flat(p))
        .collect();

    let (wallet, _) = run(&mut ma_crossover(SYMBOL, 3, 8), &candles);
    let sides: Vec<Side> = wallet.orders().iter().map(|o| o.side).collect();
    assert_eq!(
        sides.first(),
        Some(&Side::Buy),
        "first action is the golden cross"
    );
    assert!(
        sides.contains(&Side::Sell),
        "the death cross reverses to short"
    );
}

#[test]
fn rsi_reversal_buys_the_dip_and_exits_flat() {
    // Rise first so RSI warms up well above oversold, then sell off into oversold
    // (a genuine cross *below* 30 → Buy) and recover back through 50 (→ exit flat).
    //
    // The opening rise is 40 bars, not 8: a threshold cross only registers once
    // the strategy is *stable*, and RSI(5)'s Wilder seed takes ~31 extra samples
    // to settle on top of its 6-bar warm-up. An 8-bar lead-in never got past the
    // readiness gate, so under the real driver nothing traded.
    let mut prices: Vec<Real> = (0..40).map(|i| 100.0 + f64::from(i)).collect();
    prices.extend((1..=12).map(|i| 139.0 - f64::from(i) * 4.0));
    prices.extend((1..=12).map(|i| 91.0 + f64::from(i) * 4.0));
    let candles: Vec<Candle> = prices
        .iter()
        .map(|&p| common::bars::flat(p))
        .collect();

    let (wallet, _) = run(&mut rsi_reversal(SYMBOL, 5, 30.0, 50.0), &candles);
    assert!(!wallet.orders().is_empty(), "should have bought the dip");
    assert!(
        wallet.positions().is_empty(),
        "should have exited on the recovery"
    );
    let sides: Vec<Side> = wallet.orders().iter().map(|o| o.side).collect();
    assert_eq!(sides.first(), Some(&Side::Buy));
    assert_eq!(sides.last(), Some(&Side::Sell));
}

#[test]
fn reset_returns_a_strategy_to_its_initial_state() {
    let c = series();
    let mut strat = ma_crossover(SYMBOL, 5, 20);

    let (first, first_report) = run(&mut strat, &c);
    strat.reset();
    let (second, second_report) = run(&mut strat, &c);

    // After reset the strategy replays identically — same orders, and the same
    // equity path, which the order list alone would not pin.
    assert_eq!(first.orders(), second.orders());
    assert_eq!(first_report.equity_curve, second_report.equity_curve);
}
