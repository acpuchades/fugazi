//! Behavioural tests for the built-in strategy catalogue: every strategy is run,
//! one bar at a time, into a `PaperWallet` over a synthetic price path that both
//! trends (up then down) and oscillates — so trend, breakout, mean-reversion,
//! momentum, volume and composite strategies all find something to trade.

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

const SYMBOL: &str = "X";
const FUNDS: Real = 10_000.0;

/// A price path that rises for the first half and falls for the second, with a
/// steady oscillation on top — rich enough to exercise every strategy family.
fn series() -> Vec<Candle> {
    let mut candles = Vec::new();
    let mut prev_close: Real = 100.0;
    for i in 0..200i32 {
        let trend = if i < 100 {
            100.0 + f64::from(i) * 0.8
        } else {
            180.0 - f64::from(i - 100) * 0.8
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

/// Drive `strat` over `candles` into a fresh wallet and hand it back.
fn run<S>(mut strat: S, candles: &[Candle]) -> PaperWallet<&'static str>
where
    S: Strategy<Input = Atom, Symbol = &'static str>,
{
    let mut wallet = PaperWallet::new(FUNDS);
    for &candle in candles {
        for fill in wallet.update(SYMBOL, candle) {
            strat.on_fill(&fill);
        }
        strat.update(candle.into());
        strat.trade(&mut wallet);
    }
    wallet
}

/// Assert a strategy actually traded, and left the wallet in a finite state.
fn assert_trades<S>(name: &str, strat: S, candles: &[Candle])
where
    S: Strategy<Input = Atom, Symbol = &'static str>,
{
    let wallet = run(strat, candles);
    assert!(!wallet.orders().is_empty(), "{name} never traded");
    assert!(
        wallet.funds().0.is_finite(),
        "{name} produced non-finite funds"
    );
}

#[test]
fn every_strategy_trades_over_the_path() {
    let c = series();

    // Trend-following.
    assert_trades("ma_crossover", ma_crossover(SYMBOL, 5, 20), &c);
    assert_trades("macd_crossover", macd_crossover(SYMBOL, 12, 26, 9), &c);
    assert_trades("macd_zero_cross", macd_zero_cross(SYMBOL, 12, 26, 9), &c);
    assert_trades("donchian_breakout", donchian_breakout(SYMBOL, 20), &c);
    assert_trades("triple_ma", triple_ma(SYMBOL, 5, 10, 20), &c);
    assert_trades(
        "bollinger_breakout",
        bollinger_breakout(SYMBOL, 20, 2.0),
        &c,
    );

    // Mean-reversion.
    assert_trades("rsi_reversal", rsi_reversal(SYMBOL, 14, 30.0, 50.0), &c);
    assert_trades(
        "bollinger_reversion",
        bollinger_reversion(SYMBOL, 20, 2.0),
        &c,
    );
    assert_trades(
        "stochastic_reversal",
        stochastic_reversal(SYMBOL, 14, 0.2, 0.8),
        &c,
    );
    assert_trades(
        "stoch_rsi_reversal",
        stoch_rsi_reversal(SYMBOL, 14, 14, 0.2, 0.8),
        &c,
    );
    assert_trades("mfi_reversal", mfi_reversal(SYMBOL, 14, 20.0, 80.0), &c);
    assert_trades("ZScoreReversion", ZScoreReversion::new(SYMBOL, 20, 1.0), &c);

    // Momentum.
    assert_trades("momentum_roc", momentum_roc(SYMBOL, 10), &c);
    assert_trades("rsi_midline", rsi_midline(SYMBOL, 14), &c);

    // Volume / flow.
    assert_trades("obv_trend", obv_trend(SYMBOL, 20), &c);
    assert_trades("vwap_reversion", vwap_reversion(SYMBOL), &c);
    assert_trades("chaikin_ad_trend", chaikin_ad_trend(SYMBOL, 20), &c);

    // Composite.
    assert_trades(
        "adx_trend_filter",
        adx_trend_filter(SYMBOL, 5, 20, 14, 10.0),
        &c,
    );
    // A Connors-style short-period RSI: a 14-period RSI rarely pulls back to
    // oversold mid-uptrend, but RSI(2) dips hard on any down-bar.
    assert_trades("rsi_pullback", rsi_pullback(SYMBOL, 2, 20, 15.0, 60.0), &c);
    assert_trades(
        "keltner_breakout",
        keltner_breakout(SYMBOL, 20, 10, 2.0),
        &c,
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
        .map(|&p| Candle::new(p, p, p, p, 1.0))
        .collect();

    let wallet = run(ma_crossover(SYMBOL, 3, 8), &candles);
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
    // The opening rise matters: a threshold cross only registers once RSI is warm,
    // so the dip must happen after warm-up rather than coincide with it.
    let mut prices: Vec<Real> = (0..8).map(|i| 100.0 + f64::from(i)).collect();
    prices.extend((1..=12).map(|i| 107.0 - f64::from(i) * 4.0));
    prices.extend((1..=12).map(|i| 59.0 + f64::from(i) * 4.0));
    let candles: Vec<Candle> = prices
        .iter()
        .map(|&p| Candle::new(p, p, p, p, 1.0))
        .collect();

    let wallet = run(rsi_reversal(SYMBOL, 5, 30.0, 50.0), &candles);
    assert!(!wallet.orders().is_empty(), "should have bought the dip");
    assert!(
        wallet.positions().next().is_none(),
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

    let mut first = PaperWallet::new(FUNDS);
    for &candle in &c {
        for fill in first.update(SYMBOL, candle) {
            strat.on_fill(&fill);
        }
        strat.update(candle.into());
        strat.trade(&mut first);
    }

    strat.reset();
    let mut second = PaperWallet::new(FUNDS);
    for &candle in &c {
        for fill in second.update(SYMBOL, candle) {
            strat.on_fill(&fill);
        }
        strat.update(candle.into());
        strat.trade(&mut second);
    }

    // After reset the strategy replays identically.
    assert_eq!(first.orders(), second.orders());
}
