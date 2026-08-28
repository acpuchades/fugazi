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
use fugazi::indicators::{Current, Identity, Rsi, Value};
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
use fugazi::types::{Atom, Snapshot};

const SYMBOL: &str = "X";
const FUNDS: Real = 10_000.0;

type Catalogue = dyn Strategy<Input = Snapshot<&'static str>, Symbol = &'static str>;

/// Builds a fresh instance of one catalogue entry. Boxed rather than generic so
/// every entry lives in one list, and a *factory* rather than a value because
/// the assertions need two independent instances (a traded run and an untraded
/// readiness probe) and the catalogue's strategies are not `Clone`.
type Factory = Box<dyn Fn() -> Box<Catalogue>>;

/// One catalogue row: name, factory, and the two things its doc comment commits
/// to — which way it leans, and which sides it takes.
type Entry = (&'static str, Factory, Bias, Stance);

/// Half a rise-then-fall cycle, in bars. [`series`] runs four of these — rise,
/// fall, rise, fall — and [`rising`] reads a bar's regime back off the same
/// constant, so the price path and the regime split can never disagree.
const CYCLE: usize = 100;

/// Whether the underlying trend of [`series`] is rising on `bar`.
///
/// The *trend*, not the bar-to-bar change: the path carries a 10-unit
/// oscillation on top of a ±0.8/bar ramp, so individual bars fall inside a
/// rising leg and vice versa. That is deliberate — a regime that only ever
/// moved one way would let a strategy score as trend-following by never
/// trading against a single down bar.
fn rising(bar: usize) -> bool {
    bar % (2 * CYCLE) < CYCLE
}

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
    let mut candles = Vec::new();
    let mut prev_close: Real = 100.0;
    for i in 0..4 * CYCLE {
        let phase = (i % (2 * CYCLE)) as Real;
        let trend = if rising(i) {
            100.0 + phase * 0.8
        } else {
            180.0 - (phase - CYCLE as Real) * 0.8
        };
        let close = trend + 10.0 * (i as Real * 0.25).sin();
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
fn run<S>(
    strat: &mut S,
    candles: &[Candle],
) -> (PaperWallet<&'static str>, fugazi::RunReport<&'static str>)
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

/// What a catalogue entry's **documented rule** implies about *when* it holds a
/// long — the one thing the structural contract below cannot see.
///
/// Without it every assertion in [`assert_catalogue_contract`] survives
/// inverting the strategy: "it traded", "the curve is finite" and "nothing
/// filled before it was ready" all hold just as well for a rule wired
/// backwards. Eight such inversions — including dropping `donchian_breakout`'s
/// one-bar channel lag, which is a lookahead — used to pass this file
/// untouched.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Bias {
    /// Trend-following, breakout or momentum: **more long in rising legs than
    /// in falling ones**.
    WithTrend,
    /// Mean-reverting: buys weakness and sells strength, so **more long in
    /// falling legs than in rising ones**.
    AgainstTrend,
    /// Pinned by a test of its own instead. The regime average is the wrong
    /// instrument for a rule that combines an against-trend *trigger* with a
    /// with-trend *filter*: which of the two dominates the average is a
    /// property of the path's oscillation amplitude, not of the rule. See
    /// `rsi_pullback_takes_the_dip_only_in_an_uptrend`.
    Dedicated,
}

/// Which sides a catalogue entry's doc comment says it takes.
///
/// Every entry's summary line ends in one of these two phrases, so this is
/// transcription rather than judgement — and it is what catches the class of
/// break the [`Bias`] sweep cannot see: a gate wired open. A `momentum_roc`
/// whose long condition is always true still leans with the trend (it is long
/// in the rises and flat in the falls, which passes [`Bias::WithTrend`]) but it
/// has stopped being always-in, and it never shorts again.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Stance {
    /// "always-in long/short" — the entry must reach **both** sides.
    LongShort,
    /// "long/flat" — the entry must reach long *and* flat, and must **never**
    /// hold a short.
    LongFlat,
}

/// Signed exposure at the close of each bar, as a *sign* (`+1` long, `-1`
/// short, `0` flat), reconstructed from the report's fills.
///
/// The sign rather than the unit count, because a `value_frac` order buys more
/// units at 100 than at 180 — averaging raw units would score every
/// always-in strategy as biased toward whichever leg is cheaper, which is an
/// artefact of the path rather than anything the strategy decided.
fn exposure_sign_by_bar(report: &fugazi::RunReport<&'static str>, bars: usize) -> Vec<Real> {
    let mut delta = vec![0.0; bars];
    for fill in &report.fills {
        let units = match fill.order.side {
            Side::Buy => fill.order.units,
            Side::Sell => -fill.order.units,
        };
        delta[fill.bar] += units;
    }
    let mut held = 0.0;
    delta
        .into_iter()
        .map(|d| {
            held += d;
            // The quantity epsilon the wallet itself treats as flat.
            if held.abs() < 1e-9 {
                0.0
            } else {
                held.signum()
            }
        })
        .collect()
}

/// Mean exposure sign over the rising legs and over the falling legs, counting
/// only bars from `ready` on.
///
/// Bars before readiness are excluded because nothing has traded yet, and a
/// long prefix of forced zeros pulls both means toward each other — the guard
/// would weaken as an entry's warm-up grew, which is exactly backwards.
fn regime_means(exposure: &[Real], ready: usize) -> (Real, Real) {
    let mut sums = [0.0, 0.0];
    let mut counts = [0.0, 0.0];
    for (bar, &e) in exposure.iter().enumerate().skip(ready) {
        let leg = usize::from(rising(bar));
        sums[leg] += e;
        counts[leg] += 1.0;
    }
    assert!(
        counts[0] > 0.0 && counts[1] > 0.0,
        "the path must span both regimes after bar {ready}"
    );
    (sums[1] / counts[1], sums[0] / counts[0])
}

/// The catalogue-wide contract, asserted for one strategy.
///
/// `make` is called twice — once for the traded run, once for the untraded
/// readiness probe — because the catalogue's strategies are not `Clone` and the
/// probe must not be contaminated by fills.
#[track_caller]
fn assert_catalogue_contract(
    name: &str,
    make: impl Fn() -> Box<Catalogue>,
    bias: Bias,
    stance: Stance,
    candles: &[Candle],
) {
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
    // declares itself ready. This is the invariant the whole `stable_bars`
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

    // Everything above holds just as well for a rule wired backwards, so the
    // last two assertions are what the entry's own doc comment commits to: the
    // sides it takes, and the direction it leans.
    let exposure = exposure_sign_by_bar(&report, candles.len());
    let held = |side: Real| exposure.contains(&side);
    match stance {
        Stance::LongShort => {
            assert!(
                held(1.0) && held(-1.0),
                "{name} is documented as always-in long/short but only ever held \
                 {}",
                if held(1.0) { "longs" } else { "shorts" }
            );
        }
        Stance::LongFlat => {
            assert!(
                held(1.0),
                "{name} is documented as long/flat but never went long"
            );
            assert!(
                held(0.0),
                "{name} is documented as long/flat but never went flat"
            );
            assert!(
                !held(-1.0),
                "{name} is documented as long/flat but held a short position"
            );
        }
    }

    let (up, down) = regime_means(&exposure, ready);
    match bias {
        Bias::WithTrend => assert!(
            up > down,
            "{name} is documented as trend-following, but held long more in the \
             falling legs ({down:+.4}) than in the rising ones ({up:+.4})"
        ),
        Bias::AgainstTrend => assert!(
            up < down,
            "{name} is documented as mean-reverting, but held long more in the \
             rising legs ({up:+.4}) than in the falling ones ({down:+.4})"
        ),
        Bias::Dedicated => {}
    }
}

/// Every catalogue entry, as a fresh-instance factory. A new strategy is one
/// line here and inherits every assertion in [`assert_catalogue_contract`].
fn catalogue() -> Vec<Entry> {
    macro_rules! entry {
        ($name:literal, $make:expr, $bias:expr, $stance:expr) => {
            (
                $name,
                Box::new(|| Box::new($make) as Box<Catalogue>) as Factory,
                $bias,
                $stance,
            )
        };
    }
    use Bias::{AgainstTrend, Dedicated, WithTrend};
    use Stance::{LongFlat, LongShort};
    vec![
        // Trend-following.
        entry!(
            "ma_crossover",
            ma_crossover(SYMBOL, 5, 20),
            WithTrend,
            LongShort
        ),
        entry!(
            "macd_crossover",
            macd_crossover(SYMBOL, 12, 26, 9),
            WithTrend,
            LongShort
        ),
        entry!(
            "macd_zero_cross",
            macd_zero_cross(SYMBOL, 12, 26, 9),
            WithTrend,
            LongShort
        ),
        entry!(
            "donchian_breakout",
            donchian_breakout(SYMBOL, 20),
            WithTrend,
            LongShort
        ),
        entry!(
            "triple_ma",
            triple_ma(SYMBOL, 5, 10, 20),
            WithTrend,
            LongFlat
        ),
        entry!(
            "bollinger_breakout",
            bollinger_breakout(SYMBOL, 20, 2.0),
            WithTrend,
            LongShort
        ),
        // Mean-reversion.
        entry!(
            "rsi_reversal",
            rsi_reversal(SYMBOL, 14, 30.0, 50.0),
            AgainstTrend,
            LongFlat
        ),
        entry!(
            "bollinger_reversion",
            bollinger_reversion(SYMBOL, 20, 2.0),
            AgainstTrend,
            LongFlat
        ),
        entry!(
            "stochastic_reversal",
            stochastic_reversal(SYMBOL, 14, 0.2, 0.8),
            AgainstTrend,
            LongFlat
        ),
        entry!(
            "stoch_rsi_reversal",
            stoch_rsi_reversal(SYMBOL, 14, 14, 0.2, 0.8),
            AgainstTrend,
            LongFlat
        ),
        entry!(
            "mfi_reversal",
            mfi_reversal(SYMBOL, 14, 20.0, 80.0),
            AgainstTrend,
            LongFlat
        ),
        entry!(
            "ZScoreReversion",
            ZScoreReversion::new(SYMBOL, 20, 1.0),
            AgainstTrend,
            LongShort
        ),
        // Momentum.
        entry!(
            "momentum_roc",
            momentum_roc(SYMBOL, 10),
            WithTrend,
            LongShort
        ),
        entry!("rsi_midline", rsi_midline(SYMBOL, 14), WithTrend, LongShort),
        // Volume / flow.
        entry!("obv_trend", obv_trend(SYMBOL, 20), WithTrend, LongFlat),
        entry!(
            "vwap_reversion",
            vwap_reversion(SYMBOL, 20),
            AgainstTrend,
            LongFlat
        ),
        entry!(
            "chaikin_ad_trend",
            chaikin_ad_trend(SYMBOL, 20),
            WithTrend,
            LongFlat
        ),
        // Composite.
        entry!(
            "adx_trend_filter",
            adx_trend_filter(SYMBOL, 5, 20, 14, 10.0),
            WithTrend,
            LongFlat
        ),
        // A Connors-style short-period RSI: a 14-period RSI rarely pulls back to
        // oversold mid-uptrend, but RSI(2) dips hard on any down-bar.
        entry!(
            "rsi_pullback",
            rsi_pullback(SYMBOL, 2, 20, 15.0, 60.0),
            Dedicated,
            LongFlat
        ),
        entry!(
            "keltner_breakout",
            keltner_breakout(SYMBOL, 20, 10, 2.0),
            WithTrend,
            LongShort
        ),
    ]
}

#[test]
fn every_strategy_trades_over_the_path() {
    let c = series();
    for (name, make, bias, stance) in catalogue() {
        assert_catalogue_contract(name, make, bias, stance, &c);
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
    let candles: Vec<Candle> = prices.iter().map(|&p| common::bars::flat(p)).collect();

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
    let candles: Vec<Candle> = prices.iter().map(|&p| common::bars::flat(p)).collect();

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

/// `rsi_pullback` is `dip AND uptrend`, and a regime average can only see the
/// product. This pins each half: the *same* RSI dip is bought when the close is
/// above its trend SMA and refused when it is below.
///
/// Both halves matter and neither is visible to the catalogue contract —
/// dropping the trend filter still trades, still stays finite, still honours
/// readiness and still holds its documented long/flat stance; so does inverting
/// it.
#[test]
fn rsi_pullback_takes_the_dip_only_in_an_uptrend() {
    const RSI: usize = 2;
    const TREND: usize = 20;
    const OVERSOLD: Real = 15.0;

    /// A 60-bar ramp at `slope` per bar from `start`, then the *same* tail in
    /// both cases: four up bars (which drive RSI(2) to its ceiling, so the drop
    /// that follows is a genuine downward crossing rather than a level that was
    /// already there) and six down bars — the dip. Six, because Wilder
    /// smoothing takes three of them to pull RSI(2) through 15, and a crossing
    /// on the final bar has no next bar to fill on.
    ///
    /// The ramp is steep enough that its direction, not the seven-bar tail,
    /// decides which side of SMA(20) the close ends on.
    fn path(start: Real, slope: Real) -> Vec<Candle> {
        let mut prices: Vec<Real> = (0..60).map(|i| start + f64::from(i) * slope).collect();
        let mut last = *prices.last().expect("non-empty ramp");
        for _ in 0..4 {
            last += 2.0;
            prices.push(last);
        }
        for _ in 0..6 {
            last -= 3.0;
            prices.push(last);
        }
        prices.iter().map(|&p| common::bars::flat(p)).collect()
    }

    let uptrend = path(100.0, 5.0);
    let downtrend = path(400.0, -5.0);

    // The negative case is only worth anything if the dip is *there* to refuse.
    // Drive the entry's own trigger over the falling path and require it to
    // fire — otherwise "no orders" would pass for a path that never dipped.
    let mut dip = Rsi::new(Identity::new(), RSI).crosses_below(Value::new(OVERSOLD));
    let fired = downtrend
        .iter()
        .filter(|c| dip.update(c.close) == Some(true))
        .count();
    assert!(
        fired > 0,
        "the falling path must contain the same RSI dip, or the refusal below \
         passes vacuously"
    );

    let (bought, _) = run(
        &mut rsi_pullback(SYMBOL, RSI, TREND, OVERSOLD, 60.0),
        &uptrend,
    );
    let (refused, _) = run(
        &mut rsi_pullback(SYMBOL, RSI, TREND, OVERSOLD, 60.0),
        &downtrend,
    );

    assert_eq!(
        bought.orders().iter().map(|o| o.side).collect::<Vec<_>>(),
        vec![Side::Buy],
        "an RSI dip above the trend SMA is the entry this strategy exists for"
    );
    assert!(
        refused.orders().is_empty(),
        "the same dip below the trend SMA must be refused, but it traded: {:?}",
        refused.orders()
    );
}

/// A condition restated from a strategy's doc comment, evaluated over the raw
/// candles — one reading per bar.
type Condition = fn(&[Candle]) -> Vec<Option<bool>>;

/// One catalogue entry's rule, restated from its doc comment.
struct Rule {
    /// The catalogue name this row answers for.
    name: &'static str,
    make: Factory,
    /// What a `Buy` fill commits the strategy to.
    buy: Condition,
    /// What a `Sell` fill commits it to — the *short entry* for an always-in
    /// entry, the *exit* for a long/flat one. Both are the same signal object
    /// in the strategies that take both sides.
    sell: Condition,
    /// Whether the doc comment says **cross**. A level is a crossing's
    /// necessary condition but not its sufficient one: `macd_crossover` wired
    /// to compare against zero instead of its signal line still satisfies
    /// `line > signal` at every entry. Where the rule is a crossing, the
    /// condition must additionally have been *false* on the bar before.
    crossing: bool,
}

/// Drive `sig` over `candles`, one reading per bar.
fn readings<I: Indicator<Input = Atom, Output = bool>>(
    mut sig: I,
    candles: &[Candle],
) -> Vec<Option<bool>> {
    candles.iter().map(|&c| sig.update(c.into())).collect()
}

/// **Every catalogue entry's rule, restated from its doc comment**, and checked
/// against the bar each of its fills was signalled on.
///
/// The two sweeps in [`assert_catalogue_contract`] see *direction* and *sides*.
/// Neither sees a **level**: a Bollinger breakout that triggers at the middle
/// band instead of the upper one still leans with the trend and still takes both
/// sides, and an RSI midline wired to 20 instead of 50 still flips long and
/// short. This is the layer that pins those, entries *and* exits.
///
/// The conditions are composed from the library's own indicators on purpose:
/// this test is about a strategy's **wiring** — which band, which lag, which
/// threshold, which operand — while the indicators' arithmetic is pinned a
/// layer down by `tests/indicator_reference.rs` and `tests/talib_validation.rs`.
/// Building the comparison with the same operators also means it carries the
/// same tolerance the strategy does.
#[test]
fn every_entry_fires_only_where_its_documented_rule_holds() {
    use fugazi::indicators::{Adx, Bollinger, Donchian, Keltner, Macd, Mfi, Sma, Stochastic, Vwap};

    macro_rules! rule {
        ($name:literal, $make:expr, $buy:expr, $sell:expr, $crossing:literal) => {
            Rule {
                name: $name,
                make: Box::new(|| Box::new($make) as Box<Catalogue>),
                buy: $buy,
                sell: $sell,
                crossing: $crossing,
            }
        };
    }

    let rules: Vec<Rule> = vec![
        // "long when the fast SMA crosses above the slow SMA, and reverses to
        // short on the opposite cross."
        rule!(
            "ma_crossover",
            ma_crossover(SYMBOL, 5, 20),
            |c| readings(
                Sma::new(Current::close(), 5).gt(Sma::new(Current::close(), 20)),
                c
            ),
            |c| readings(
                Sma::new(Current::close(), 5).lt(Sma::new(Current::close(), 20)),
                c
            ),
            true
        ),
        // "long when the MACD line crosses above its signal line, short on the
        // opposite cross."
        rule!(
            "macd_crossover",
            macd_crossover(SYMBOL, 12, 26, 9),
            |c| {
                let m = Macd::new(Current::close(), 12, 26, 9).shared();
                readings(m.line().gt(m.signal()), c)
            },
            |c| {
                let m = Macd::new(Current::close(), 12, 26, 9).shared();
                readings(m.line().lt(m.signal()), c)
            },
            true
        ),
        // "long while the MACD line is above zero, short below it, flipping on
        // the zero crossing."
        rule!(
            "macd_zero_cross",
            macd_zero_cross(SYMBOL, 12, 26, 9),
            |c| readings(Macd::new(Current::close(), 12, 26, 9).line().above(0.0), c),
            |c| readings(Macd::new(Current::close(), 12, 26, 9).line().below(0.0), c),
            true
        ),
        // "long when the close breaks above the highest high of the **prior**
        // `period` bars" — the `.lag(1)` is the difference between a breakout
        // and a lookahead, which is why this row spells the lag out.
        rule!(
            "donchian_breakout",
            donchian_breakout(SYMBOL, 20),
            |c| {
                let ch = Donchian::new(Current::high(), Current::low(), 20).shared();
                readings(Current::close().gt(ch.upper().lag(1)), c)
            },
            |c| {
                let ch = Donchian::new(Current::high(), Current::low(), 20).shared();
                readings(Current::close().lt(ch.lower().lag(1)), c)
            },
            false
        ),
        // "holds a long only while the three SMAs are stacked bullishly,
        // flattening as soon as that alignment breaks."
        rule!(
            "triple_ma",
            triple_ma(SYMBOL, 5, 10, 20),
            |c| readings(triple_ma_aligned(), c),
            |c| readings(triple_ma_aligned().not(), c),
            false
        ),
        // "long above the upper band, short below the lower one."
        rule!(
            "bollinger_breakout",
            bollinger_breakout(SYMBOL, 20, 2.0),
            |c| {
                let b = Bollinger::new(Current::close(), 20, 2.0).shared();
                readings(Current::close().gt(b.upper()), c)
            },
            |c| {
                let b = Bollinger::new(Current::close(), 20, 2.0).shared();
                readings(Current::close().lt(b.lower()), c)
            },
            false
        ),
        // "buys the dip" — RSI crossing down through `oversold` — "and exits
        // once it recovers back through `exit_level`."
        rule!(
            "rsi_reversal",
            rsi_reversal(SYMBOL, 14, 30.0, 50.0),
            |c| readings(Rsi::new(Current::close(), 14).below(30.0), c),
            |c| readings(Rsi::new(Current::close(), 14).above(50.0), c),
            true
        ),
        // "long when the close crosses below the lower band, out when it
        // crosses back above the middle one."
        rule!(
            "bollinger_reversion",
            bollinger_reversion(SYMBOL, 20, 2.0),
            |c| {
                let b = Bollinger::new(Current::close(), 20, 2.0).shared();
                readings(Current::close().lt(b.lower()), c)
            },
            |c| {
                let b = Bollinger::new(Current::close(), 20, 2.0).shared();
                readings(Current::close().gt(b.middle()), c)
            },
            true
        ),
        rule!(
            "stochastic_reversal",
            stochastic_reversal(SYMBOL, 14, 0.2, 0.8),
            |c| readings(Stochastic::new(Current::close(), 14).below(0.2), c),
            |c| readings(Stochastic::new(Current::close(), 14).above(0.8), c),
            true
        ),
        rule!(
            "stoch_rsi_reversal",
            stoch_rsi_reversal(SYMBOL, 14, 14, 0.2, 0.8),
            |c| readings(stoch_rsi().below(0.2), c),
            |c| readings(stoch_rsi().above(0.8), c),
            true
        ),
        rule!(
            "mfi_reversal",
            mfi_reversal(SYMBOL, 14, 20.0, 80.0),
            |c| readings(Mfi::new(Current::candle(), 14).below(20.0), c),
            |c| readings(Mfi::new(Current::candle(), 14).above(80.0), c),
            true
        ),
        // "long when `z <= -entry`, short when `z >= entry`, flattening once `z`
        // reverts back through zero" — a Buy is either the long entry or a short
        // being closed, and `z <= 0` is what both commit to (and vice versa).
        rule!(
            "ZScoreReversion",
            ZScoreReversion::new(SYMBOL, 20, 1.0),
            |c| readings(zscore().below(0.0), c),
            |c| readings(zscore().above(0.0), c),
            false
        ),
        // "long while the `period`-bar percentage change of the close is
        // positive, short while it is negative."
        rule!(
            "momentum_roc",
            momentum_roc(SYMBOL, 10),
            |c| readings(Current::close().roc(10).above(0.0), c),
            |c| readings(Current::close().roc(10).below(0.0), c),
            false
        ),
        // "long while RSI is above 50, short while below."
        rule!(
            "rsi_midline",
            rsi_midline(SYMBOL, 14),
            |c| readings(Rsi::new(Current::close(), 14).above(50.0), c),
            |c| readings(Rsi::new(Current::close(), 14).below(50.0), c),
            false
        ),
        // "long while OBV is above its SMA, flat below it."
        rule!(
            "obv_trend",
            obv_trend(SYMBOL, 20),
            |c| readings(obv_bullish(), c),
            |c| readings(obv_bullish().not(), c),
            false
        ),
        // "buys when price dips below a rolling VWAP and exits when it recovers
        // above."
        rule!(
            "vwap_reversion",
            vwap_reversion(SYMBOL, 20),
            |c| readings(Current::close().lt(Vwap::new(Current::candle(), 20)), c),
            |c| readings(Current::close().gt(Vwap::new(Current::candle(), 20)), c),
            true
        ),
        // "long while the A/D line is above its moving average, flat below."
        rule!(
            "chaikin_ad_trend",
            chaikin_ad_trend(SYMBOL, 20),
            |c| readings(ad_bullish(), c),
            |c| readings(ad_bullish().not(), c),
            false
        ),
        // "takes the golden cross **only when** ADX is above `adx_min`, and
        // exits on the death cross."
        rule!(
            "adx_trend_filter",
            adx_trend_filter(SYMBOL, 5, 20, 14, 10.0),
            |c| {
                readings(
                    Sma::new(Current::close(), 5)
                        .gt(Sma::new(Current::close(), 20))
                        .and(Adx::new(Current::candle(), 14).adx().above(10.0)),
                    c,
                )
            },
            |c| readings(
                Sma::new(Current::close(), 5).lt(Sma::new(Current::close(), 20)),
                c
            ),
            true
        ),
        // "buys an RSI dip **only while** the close is above its trend SMA, and
        // exits when RSI recovers up through `exit_level`."
        rule!(
            "rsi_pullback",
            rsi_pullback(SYMBOL, 2, 20, 15.0, 60.0),
            |c| {
                readings(
                    Rsi::new(Current::close(), 2)
                        .below(15.0)
                        .and(Current::close().gt(Sma::new(Current::close(), 20))),
                    c,
                )
            },
            |c| readings(Rsi::new(Current::close(), 2).above(60.0), c),
            true
        ),
        // "long when the close pierces the upper Keltner band, short below the
        // lower one."
        rule!(
            "keltner_breakout",
            keltner_breakout(SYMBOL, 20, 10, 2.0),
            |c| {
                let k = Keltner::new(Current::close(), Current::candle(), 20, 10, 2.0).shared();
                readings(Current::close().gt(k.upper()), c)
            },
            |c| {
                let k = Keltner::new(Current::close(), Current::candle(), 20, 10, 2.0).shared();
                readings(Current::close().lt(k.lower()), c)
            },
            false
        ),
    ];

    // Every catalogue entry needs a row, or a rule could be rewired in the one
    // place this file does not look.
    let named: std::collections::BTreeSet<&str> = rules.iter().map(|r| r.name).collect();
    let expected: std::collections::BTreeSet<&str> = catalogue().iter().map(|(n, ..)| *n).collect();
    assert_eq!(named, expected, "rule rows and catalogue entries disagree");

    let candles = series();
    for rule in rules {
        let name = rule.name;
        let (_, report) = run(&mut *(rule.make)(), &candles);
        let buy = (rule.buy)(&candles);
        let sell = (rule.sell)(&candles);
        let mut checked = 0;
        for fill in &report.fills {
            assert_eq!(
                fill.order.kind,
                fugazi::OrderKind::Market,
                "{name}: the catalogue rests no protective legs, so every fill \
                 is signalled on the bar before it"
            );
            // A market order queued on bar `b` fills at `b + 1`'s open.
            let signal = fill.bar.checked_sub(1).expect("no fill lands on bar 0");
            let cond = match fill.order.side {
                Side::Buy => &buy,
                Side::Sell => &sell,
            };
            assert_eq!(
                cond[signal],
                Some(true),
                "{name}: a {:?} filled on bar {} but its documented condition did \
                 not hold on the bar that signalled it ({signal})",
                fill.order.side,
                fill.bar,
            );
            if rule.crossing && signal > 0 {
                assert_ne!(
                    cond[signal - 1],
                    Some(true),
                    "{name}: a {:?} filled on bar {} on a rule documented as a \
                     *crossing*, but the condition was already true on bar {}",
                    fill.order.side,
                    fill.bar,
                    signal - 1,
                );
            }
            checked += 1;
        }
        assert!(checked > 0, "{name}: no fill was checked");
    }
}

/// `triple_ma`'s alignment, `obv_trend`'s and `chaikin_ad_trend`'s "above its
/// own MA", the StochRSI and the z-score — each is used twice above (once as
/// the entry, once negated or mirrored as the exit), and each is a mouthful.
fn triple_ma_aligned() -> impl Indicator<Input = Atom, Output = bool> {
    use fugazi::indicators::Sma;
    Sma::new(Current::close(), 5)
        .gt(Sma::new(Current::close(), 10))
        .and(Sma::new(Current::close(), 10).gt(Sma::new(Current::close(), 20)))
}

fn obv_bullish() -> impl Indicator<Input = Atom, Output = bool> {
    use fugazi::indicators::{Obv, Sma};
    Obv::new(Current::candle()).gt(Sma::new(Obv::new(Current::candle()), 20))
}

fn ad_bullish() -> impl Indicator<Input = Atom, Output = bool> {
    use fugazi::indicators::{Ad, Sma};
    Ad::new(Current::candle()).gt(Sma::new(Ad::new(Current::candle()), 20))
}

fn stoch_rsi() -> impl Indicator<Input = Atom, Output = Real> {
    use fugazi::indicators::Stochastic;
    Stochastic::new(Rsi::new(Current::close(), 14), 14)
}

fn zscore() -> impl Indicator<Input = Atom, Output = Real> {
    use fugazi::indicators::{Sma, StdDev};
    Current::close()
        .sub(Sma::new(Current::close(), 20))
        .div(StdDev::new(Current::close(), 20))
}

/// `adx_trend_filter` is `golden cross AND ADX > adx_min`, and on a trending
/// path the gate is open at every crossing — so the rule sweep above sees the
/// conjunction pass whether or not the gate is wired at all. This moves the
/// threshold instead of the path: a gate that is read must block everything
/// when it is set out of reach.
///
/// The `adx_min = 0` half is what stops the other from passing vacuously.
#[test]
fn adx_trend_filter_actually_reads_its_strength_gate() {
    let c = series();
    let (open_gate, _) = run(&mut adx_trend_filter(SYMBOL, 5, 20, 14, 0.0), &c);
    let (shut_gate, _) = run(&mut adx_trend_filter(SYMBOL, 5, 20, 14, 1.0e9), &c);

    assert!(
        !open_gate.orders().is_empty(),
        "with the gate wide open the crossings themselves must still trade"
    );
    assert!(
        shut_gate.orders().is_empty(),
        "ADX is a bounded 0..100 reading, so a threshold of 1e9 can never be \
         cleared — every one of these {} orders took a crossing the gate should \
         have refused",
        shut_gate.orders().len()
    );
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
