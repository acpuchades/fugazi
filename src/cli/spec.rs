//! Declarative, serde-deserializable mirror of the fugazi composition API.
//!
//! These spec types are the YAML *surface*: each variant maps to one fugazi
//! constructor, and `build()` turns a spec tree into the corresponding live
//! (type-erased) indicator, signal or strategy. Keeping the serde boilerplate
//! here — on dedicated wrapper enums — means the core crate's data model stays
//! free of serde and of any runtime-dispatch concession.
//!
//! Three layers, mirroring the crate:
//!
//! * [`SourceSpec`] → [`DynValue`] — a real-valued source (`Output = Real`).
//! * [`SignalSpec`] → [`DynSignal`] — a boolean condition (a `Signal`).
//! * [`StrategySpec`] → [`SingleAssetStrategy`] — the decision layer.
//!
//! The enums are *externally tagged* (serde's default), so an indicator reads as
//! a single-key map — `{ema: {source: close, period: 20}}` — and a parameterless
//! leaf or bar indicator reads as a bare string — `close`, `obv`.

use serde::Deserialize;

use fugazi::indicators::{
    Ad, Adx, AdxValue, Aroon, AroonValue, Atr, Bollinger, BollingerValue, Cci, Component, Current,
    CurrentBar, DEFAULT_EPSILON, Dmi, DmiValue, Donchian, DonchianValue, Ema, Hma, Keltner,
    KeltnerValue, Latch, Macd, MacdValue, Mfi, Obv, Position, Resample, Rma, Rsi, Sar, Sma, StdDev,
    StochRsi, Stochastic, TrueRange, Value, Vwap, WilliamsR, Wma,
};
use fugazi::indicators::compare;
use fugazi::indicators::logic::Const;
use fugazi::prelude::*;
use fugazi::strategies::SingleAssetStrategy;

use crate::dyn_::{self, AsBool, AsReal, DynIndicator};

fn default_source() -> Box<SourceSpec> {
    Box::new(SourceSpec::Close)
}
fn default_high() -> Box<SourceSpec> {
    Box::new(SourceSpec::High)
}
fn default_low() -> Box<SourceSpec> {
    Box::new(SourceSpec::Low)
}

// ---------------------------------------------------------------------------
// Real-valued sources
// ---------------------------------------------------------------------------

/// A real-valued source over a candle stream — the YAML form of any
/// `Indicator<Input = Candle, Output = Real>`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceSpec {
    // --- candle-field leaves (bare strings) ---
    Close,
    High,
    Low,
    Open,
    Volume,
    Typical,
    Median,
    /// A constant value.
    Value(Real),
    /// The current position's entry price — a [`SingleAssetStrategy`] anchor,
    /// for building stop-loss / take-profit levels.
    Entry,
    /// The running high since entry (a long trailing-stop anchor).
    Peak,
    /// The running low since entry (a short trailing-stop anchor).
    Trough,

    // --- price-series indicators (a source + parameters) ---
    Ema {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        period: usize,
    },
    Sma {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        period: usize,
    },
    Rma {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        period: usize,
    },
    Wma {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        period: usize,
    },
    Hma {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        period: usize,
    },
    Rsi {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        period: usize,
    },
    #[serde(rename = "stddev")]
    StdDev {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        period: usize,
    },
    Cci {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        period: usize,
    },
    Stochastic {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        period: usize,
    },
    StochRsi {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        rsi_period: usize,
        stoch_period: usize,
    },

    // --- multi-output indicators, one variant per component ---
    MacdLine {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        fast: usize,
        slow: usize,
        signal: usize,
    },
    MacdSignal {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        fast: usize,
        slow: usize,
        signal: usize,
    },
    MacdHistogram {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        fast: usize,
        slow: usize,
        signal: usize,
    },
    BbUpper {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        period: usize,
        k: Real,
    },
    BbMiddle {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        period: usize,
        k: Real,
    },
    BbLower {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        period: usize,
        k: Real,
    },
    KeltnerUpper {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        ema_period: usize,
        atr_period: usize,
        multiplier: Real,
    },
    KeltnerMiddle {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        ema_period: usize,
        atr_period: usize,
        multiplier: Real,
    },
    KeltnerLower {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        ema_period: usize,
        atr_period: usize,
        multiplier: Real,
    },
    DonchianUpper {
        #[serde(default = "default_high")]
        high: Box<SourceSpec>,
        #[serde(default = "default_low")]
        low: Box<SourceSpec>,
        period: usize,
    },
    DonchianMiddle {
        #[serde(default = "default_high")]
        high: Box<SourceSpec>,
        #[serde(default = "default_low")]
        low: Box<SourceSpec>,
        period: usize,
    },
    DonchianLower {
        #[serde(default = "default_high")]
        high: Box<SourceSpec>,
        #[serde(default = "default_low")]
        low: Box<SourceSpec>,
        period: usize,
    },
    Adx {
        period: usize,
    },
    PlusDi {
        period: usize,
    },
    MinusDi {
        period: usize,
    },
    DmiPlusDi {
        period: usize,
    },
    DmiMinusDi {
        period: usize,
    },
    AroonUp {
        period: usize,
    },
    AroonDown {
        period: usize,
    },
    AroonOscillator {
        period: usize,
    },

    // --- single-output bar indicators ---
    Atr {
        period: usize,
    },
    Mfi {
        period: usize,
    },
    WilliamsR {
        period: usize,
    },
    Obv,
    Vwap,
    Ad,
    TrueRange,
    Sar {
        step: Real,
        max: Real,
    },

    // --- transform operators ---
    Add {
        lhs: Box<SourceSpec>,
        rhs: Box<SourceSpec>,
    },
    Sub {
        lhs: Box<SourceSpec>,
        rhs: Box<SourceSpec>,
    },
    Mul {
        lhs: Box<SourceSpec>,
        rhs: Box<SourceSpec>,
    },
    Div {
        lhs: Box<SourceSpec>,
        rhs: Box<SourceSpec>,
    },
    Lag {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        periods: usize,
    },
    Diff {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        periods: usize,
    },
    Ratio {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        periods: usize,
    },
    Roc {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        periods: usize,
    },
    RollingMax {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        period: usize,
    },
    RollingMin {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        period: usize,
    },
    /// Holds the most recent `Some` output of `source`, re-emitting it on
    /// ticks where `source` returns `None`. Wrap the outermost recursive
    /// smoother of a resampled pipeline so per-base-tick consumers see the
    /// finished higher-timeframe value between boundaries — see
    /// [`fugazi::indicators::Latch`].
    Latch { source: Box<SourceSpec> },
    /// Aggregates `every` base candles into one higher-timeframe candle and
    /// runs the `inner` source over it, emitting `inner`'s output on each
    /// completed bucket and `None` in between. `inner` is any source that
    /// reads a candle (`close`/`high`/`typical`, `!ema { period: N, source:
    /// close }`, `!add { lhs, rhs }`, …); it advances only on emissions from
    /// the resample, so an `!ema` inside `!resample` recurses over the HTF
    /// closes, not the base ones. Wrap the whole downstream chain in
    /// [`Latch`](SourceSpec::Latch) so per-base-tick reads see the finished
    /// value between boundaries.
    Resample { every: usize, inner: Box<SourceSpec> },
}

impl SourceSpec {
    /// Construct the live, runtime-typed source this spec describes as a
    /// `Box<dyn DynIndicator>` with `output_type() == DynType::Real`. `anchor`
    /// is the owning strategy's [`Position`], shared by any `entry` / `peak` /
    /// `trough` leaves in the tree.
    pub fn build(&self, anchor: &Position) -> Box<dyn DynIndicator> {
        use SourceSpec::*;
        // Recursive-build shorthand: build `s`, view it as a library-typed
        // `Indicator<Input=Candle, Output=Real>` so it drops into a concrete
        // library constructor.
        let real = |s: &SourceSpec| AsReal::new(s.build(anchor));

        match self {
            Close => dyn_::wrap(Current::close()),
            High => dyn_::wrap(Current::high()),
            Low => dyn_::wrap(Current::low()),
            Open => dyn_::wrap(Current::open()),
            Volume => dyn_::wrap(Current::volume()),
            Typical => dyn_::wrap(Current::typical()),
            Median => dyn_::wrap(Current::median()),
            Value(x) => dyn_::wrap(self::Value::<Candle>::new(*x)),
            Entry => dyn_::wrap(anchor.entry()),
            Peak => dyn_::wrap(anchor.peak()),
            Trough => dyn_::wrap(anchor.trough()),

            Ema { source, period } => dyn_::wrap(self::Ema::new(real(source), *period)),
            Sma { source, period } => dyn_::wrap(self::Sma::new(real(source), *period)),
            Rma { source, period } => dyn_::wrap(self::Rma::new(real(source), *period)),
            Wma { source, period } => dyn_::wrap(self::Wma::new(real(source), *period)),
            Hma { source, period } => dyn_::wrap(self::Hma::new(real(source), *period)),
            Rsi { source, period } => dyn_::wrap(self::Rsi::new(real(source), *period)),
            StdDev { source, period } => dyn_::wrap(self::StdDev::new(real(source), *period)),
            Cci { source, period } => dyn_::wrap(self::Cci::new(real(source), *period)),
            Stochastic { source, period } => {
                dyn_::wrap(self::Stochastic::new(real(source), *period))
            }
            StochRsi {
                source,
                rsi_period,
                stoch_period,
            } => dyn_::wrap(self::StochRsi::new(
                self::Rsi::new(real(source), *rsi_period),
                *stoch_period,
            )),

            MacdLine {
                source,
                fast,
                slow,
                signal,
            } => dyn_::wrap(Component::new(
                Macd::new(real(source), *fast, *slow, *signal),
                |v: MacdValue| v.macd,
            )),
            MacdSignal {
                source,
                fast,
                slow,
                signal,
            } => dyn_::wrap(Component::new(
                Macd::new(real(source), *fast, *slow, *signal),
                |v: MacdValue| v.signal,
            )),
            MacdHistogram {
                source,
                fast,
                slow,
                signal,
            } => dyn_::wrap(Component::new(
                Macd::new(real(source), *fast, *slow, *signal),
                |v: MacdValue| v.histogram,
            )),

            BbUpper { source, period, k } => dyn_::wrap(Component::new(
                Bollinger::new(real(source), *period, *k),
                |v: BollingerValue| v.upper,
            )),
            BbMiddle { source, period, k } => dyn_::wrap(Component::new(
                Bollinger::new(real(source), *period, *k),
                |v: BollingerValue| v.middle,
            )),
            BbLower { source, period, k } => dyn_::wrap(Component::new(
                Bollinger::new(real(source), *period, *k),
                |v: BollingerValue| v.lower,
            )),

            KeltnerUpper {
                source,
                ema_period,
                atr_period,
                multiplier,
            } => dyn_::wrap(Component::new(
                Keltner::new(real(source), *ema_period, *atr_period, *multiplier),
                |v: KeltnerValue| v.upper,
            )),
            KeltnerMiddle {
                source,
                ema_period,
                atr_period,
                multiplier,
            } => dyn_::wrap(Component::new(
                Keltner::new(real(source), *ema_period, *atr_period, *multiplier),
                |v: KeltnerValue| v.middle,
            )),
            KeltnerLower {
                source,
                ema_period,
                atr_period,
                multiplier,
            } => dyn_::wrap(Component::new(
                Keltner::new(real(source), *ema_period, *atr_period, *multiplier),
                |v: KeltnerValue| v.lower,
            )),

            DonchianUpper { high, low, period } => dyn_::wrap(Component::new(
                Donchian::new(real(high), real(low), *period),
                |v: DonchianValue| v.upper,
            )),
            DonchianMiddle { high, low, period } => dyn_::wrap(Component::new(
                Donchian::new(real(high), real(low), *period),
                |v: DonchianValue| v.middle,
            )),
            DonchianLower { high, low, period } => dyn_::wrap(Component::new(
                Donchian::new(real(high), real(low), *period),
                |v: DonchianValue| v.lower,
            )),

            Adx { period } => {
                dyn_::wrap(Component::new(self::Adx::new(*period), |v: AdxValue| v.adx))
            }
            PlusDi { period } => dyn_::wrap(Component::new(
                self::Adx::new(*period),
                |v: AdxValue| v.plus_di,
            )),
            MinusDi { period } => dyn_::wrap(Component::new(
                self::Adx::new(*period),
                |v: AdxValue| v.minus_di,
            )),
            DmiPlusDi { period } => dyn_::wrap(Component::new(
                self::Dmi::new(*period),
                |v: DmiValue| v.plus_di,
            )),
            DmiMinusDi { period } => dyn_::wrap(Component::new(
                self::Dmi::new(*period),
                |v: DmiValue| v.minus_di,
            )),

            AroonUp { period } => dyn_::wrap(Component::new(
                self::Aroon::new(*period),
                |v: AroonValue| v.up,
            )),
            AroonDown { period } => dyn_::wrap(Component::new(
                self::Aroon::new(*period),
                |v: AroonValue| v.down,
            )),
            AroonOscillator { period } => dyn_::wrap(Component::new(
                self::Aroon::new(*period),
                |v: AroonValue| v.oscillator,
            )),

            Atr { period } => dyn_::wrap(self::Atr::new(*period)),
            Mfi { period } => dyn_::wrap(self::Mfi::new(*period)),
            WilliamsR { period } => dyn_::wrap(self::WilliamsR::new(*period)),
            Obv => dyn_::wrap(self::Obv::new()),
            Vwap => dyn_::wrap(self::Vwap::new()),
            Ad => dyn_::wrap(self::Ad::new()),
            TrueRange => dyn_::wrap(self::TrueRange::new()),
            Sar { step, max } => dyn_::wrap(self::Sar::new(*step, *max)),

            Add { lhs, rhs } => dyn_::wrap(real(lhs).add(real(rhs))),
            Sub { lhs, rhs } => dyn_::wrap(real(lhs).sub(real(rhs))),
            Mul { lhs, rhs } => dyn_::wrap(real(lhs).mul(real(rhs))),
            Div { lhs, rhs } => dyn_::wrap(real(lhs).div(real(rhs))),
            Lag { source, periods } => dyn_::wrap(real(source).lag(*periods)),
            Diff { source, periods } => dyn_::wrap(real(source).diff(*periods)),
            Ratio { source, periods } => dyn_::wrap(real(source).ratio(*periods)),
            Roc { source, periods } => dyn_::wrap(real(source).roc(*periods)),
            RollingMax { source, period } => dyn_::wrap(real(source).rolling_max(*period)),
            RollingMin { source, period } => dyn_::wrap(real(source).rolling_min(*period)),
            Latch { source } => {
                // Wrap the built source in the library's Latch — preserves
                // Real output; warm-up / unstable pass through unchanged.
                let inner = AsReal::new(source.build(anchor));
                dyn_::wrap(self::Latch::new(inner))
            }
            Resample { every, inner } => {
                assert!(*every > 0, "resample every must be greater than zero");
                let resample = self::Resample::new(CurrentBar::new(), *every);
                let inner_typed = AsReal::new(inner.build(anchor));
                dyn_::wrap(ResampleThen::new(resample, inner_typed))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ResampleThen: run an inner Candle→Real source over the higher-timeframe
// candles emitted by a Resample. The inner advances only on Resample
// emissions, so an Ema-P inside `!resample { every, inner }` recurses over
// the HTF closes (not the base ones) and its warm-up / unstable-period scale
// by `every` in base-bar terms.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct ResampleThen<Inner> {
    resample: Resample<CurrentBar>,
    inner: Inner,
    value: Option<Real>,
}

impl<Inner> ResampleThen<Inner> {
    fn new(resample: Resample<CurrentBar>, inner: Inner) -> Self {
        Self {
            resample,
            inner,
            value: None,
        }
    }
}

impl<Inner> Indicator for ResampleThen<Inner>
where
    Inner: Indicator<Input = Candle, Output = Real>,
{
    type Input = Candle;
    type Output = Real;

    fn update(&mut self, c: Candle) -> Option<Real> {
        self.value = match self.resample.update(c) {
            Some(htf) => self.inner.update(htf),
            None => None,
        };
        self.value
    }

    fn value(&self) -> Option<Real> {
        self.value
    }

    fn warm_up_period(&self) -> usize {
        // The k-th HTF candle arrives at base sample `every * k` (given
        // Resample<CurrentBar>::warm_up_period() == every). The inner needs
        // `inner.warm_up_period()` HTF samples to be ready, so its first
        // `Some` arrives at base sample `every * inner.warm_up_period()`.
        self.resample.every() * self.inner.warm_up_period().max(1)
    }

    fn unstable_period(&self) -> usize {
        // Inner's unstable period is in HTF-sample units; convert to base
        // bars by scaling with `every`.
        self.inner.unstable_period() * self.resample.every()
    }

    fn reset(&mut self) {
        self.resample.reset();
        self.inner.reset();
        self.value = None;
    }
}

// ---------------------------------------------------------------------------
// Boolean signals
// ---------------------------------------------------------------------------

/// A boolean condition over a candle stream — the YAML form of a `Signal`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalSpec {
    // --- comparisons ---
    Gt {
        lhs: Box<SourceSpec>,
        rhs: Box<SourceSpec>,
        epsilon: Option<Real>,
    },
    Lt {
        lhs: Box<SourceSpec>,
        rhs: Box<SourceSpec>,
        epsilon: Option<Real>,
    },
    Ge {
        lhs: Box<SourceSpec>,
        rhs: Box<SourceSpec>,
        epsilon: Option<Real>,
    },
    Le {
        lhs: Box<SourceSpec>,
        rhs: Box<SourceSpec>,
        epsilon: Option<Real>,
    },
    Eq {
        lhs: Box<SourceSpec>,
        rhs: Box<SourceSpec>,
        epsilon: Option<Real>,
    },
    Ne {
        lhs: Box<SourceSpec>,
        rhs: Box<SourceSpec>,
        epsilon: Option<Real>,
    },
    /// `source > level` against a constant.
    Above {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        level: Real,
    },
    /// `source < level` against a constant.
    Below {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        level: Real,
    },

    // --- crossovers ---
    CrossesAbove {
        lhs: Box<SourceSpec>,
        rhs: Box<SourceSpec>,
    },
    CrossesBelow {
        lhs: Box<SourceSpec>,
        rhs: Box<SourceSpec>,
    },

    // --- boolean logic ---
    And {
        lhs: Box<SignalSpec>,
        rhs: Box<SignalSpec>,
    },
    Or {
        lhs: Box<SignalSpec>,
        rhs: Box<SignalSpec>,
    },
    Xor {
        lhs: Box<SignalSpec>,
        rhs: Box<SignalSpec>,
    },
    /// AND-fold of a list (empty ⇒ constant `true`).
    All(Vec<SignalSpec>),
    /// OR-fold of a list (empty ⇒ constant `false`).
    Any(Vec<SignalSpec>),
    Not(Box<SignalSpec>),
    Changed(Box<SignalSpec>),
    /// Reports whether `signal`'s chain has been fed at least its
    /// `stable_period()` samples. Compose in an `!and` with an entry signal to
    /// gate the entry on stability (see
    /// [`fugazi::indicators::Stable`]).
    Stable { signal: Box<SignalSpec> },
    /// A constant boolean leaf. Spelled `!value` like [`SourceSpec::Value`] —
    /// one tag for "a literal", typed by position (bool here, number there).
    Value(bool),
}

/// Resolve an optional tolerance to its concrete value.
fn eps(epsilon: &Option<Real>) -> Real {
    epsilon.unwrap_or(DEFAULT_EPSILON)
}

impl SignalSpec {
    /// Construct the live, runtime-typed signal this spec describes as a
    /// `Box<dyn DynIndicator>` with `output_type() == DynType::Bool`. `anchor`
    /// is threaded to any `entry` / `peak` / `trough` source leaf.
    pub fn build(&self, anchor: &Position) -> Box<dyn DynIndicator> {
        use SignalSpec::*;
        let real = |s: &SourceSpec| AsReal::new(s.build(anchor));
        let boolean = |s: &SignalSpec| AsBool::new(s.build(anchor));

        match self {
            Gt { lhs, rhs, epsilon } => dyn_::wrap(compare::Gt::with_epsilon(
                real(lhs),
                real(rhs),
                eps(epsilon),
            )),
            Lt { lhs, rhs, epsilon } => dyn_::wrap(compare::Lt::with_epsilon(
                real(lhs),
                real(rhs),
                eps(epsilon),
            )),
            Ge { lhs, rhs, epsilon } => dyn_::wrap(compare::Ge::with_epsilon(
                real(lhs),
                real(rhs),
                eps(epsilon),
            )),
            Le { lhs, rhs, epsilon } => dyn_::wrap(compare::Le::with_epsilon(
                real(lhs),
                real(rhs),
                eps(epsilon),
            )),
            Eq { lhs, rhs, epsilon } => dyn_::wrap(compare::Eq::with_epsilon(
                real(lhs),
                real(rhs),
                eps(epsilon),
            )),
            Ne { lhs, rhs, epsilon } => dyn_::wrap(compare::Ne::with_epsilon(
                real(lhs),
                real(rhs),
                eps(epsilon),
            )),
            Above { source, level } => dyn_::wrap(real(source).above(*level)),
            Below { source, level } => dyn_::wrap(real(source).below(*level)),

            // A crossover clones its operands (the `Change` half needs a fresh
            // comparison state); rebuild each operand from the spec so we get
            // two independently-advanced instances.
            CrossesAbove { lhs, rhs } => {
                let cmp = || real(lhs).gt(real(rhs));
                dyn_::wrap(cmp().and(cmp().changed()))
            }
            CrossesBelow { lhs, rhs } => {
                let cmp = || real(lhs).lt(real(rhs));
                dyn_::wrap(cmp().and(cmp().changed()))
            }

            And { lhs, rhs } => dyn_::wrap(boolean(lhs).and(boolean(rhs))),
            Or { lhs, rhs } => dyn_::wrap(boolean(lhs).or(boolean(rhs))),
            Xor { lhs, rhs } => dyn_::wrap(boolean(lhs).xor(boolean(rhs))),
            All(specs) => {
                if specs.is_empty() {
                    dyn_::wrap(self::Const::<Candle>::new(true))
                } else {
                    let mut acc = AsBool::new(specs[0].build(anchor));
                    for s in &specs[1..] {
                        let next = AsBool::new(s.build(anchor));
                        // AsBool `and` AsBool → concrete Combine; wrap in AsBool
                        // by round-tripping through the box so the fold's accumulator
                        // stays a single library type.
                        acc = AsBool::new(dyn_::wrap(acc.and(next)));
                    }
                    dyn_::wrap(acc)
                }
            }
            Any(specs) => {
                if specs.is_empty() {
                    dyn_::wrap(self::Const::<Candle>::new(false))
                } else {
                    let mut acc = AsBool::new(specs[0].build(anchor));
                    for s in &specs[1..] {
                        let next = AsBool::new(s.build(anchor));
                        acc = AsBool::new(dyn_::wrap(acc.or(next)));
                    }
                    dyn_::wrap(acc)
                }
            }
            Not(inner) => dyn_::wrap(boolean(inner).not()),
            Changed(inner) => dyn_::wrap(boolean(inner).changed()),
            Stable { signal } => dyn_::stable_check(signal.build(anchor).stable_period()),
            Value(b) => dyn_::wrap(self::Const::<Candle>::new(*b)),
        }
    }
}

// ---------------------------------------------------------------------------
// Strategy
// ---------------------------------------------------------------------------

/// One side of a [`SingleAssetStrategy`]: the entry condition and an optional
/// exit.
///
/// `exit` defaults to a constant-`false` signal. Omitting it is exactly right for
/// an always-in long/short reversal — the opposite side's `enter` already
/// reverses the position, so an explicit flatten-to-flat exit would be dead. Give
/// a side an `exit` only when you want a flat rest (long/flat, or long/short with
/// a flat state between trades).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SideSpec {
    pub enter: SignalSpec,
    #[serde(default)]
    pub exit: Option<SignalSpec>,
    /// An optional stop-loss price level (a source). The side flattens when the
    /// adverse extreme of the bar reaches it. A `peak` / `trough` source makes it
    /// a trailing stop.
    #[serde(default)]
    pub stop_loss: Option<Box<SourceSpec>>,
    /// An optional take-profit price level (a source). The side flattens when the
    /// favourable extreme of the bar reaches it.
    #[serde(default)]
    pub take_profit: Option<Box<SourceSpec>>,
}

impl SideSpec {
    /// Build this side's exit signal, defaulting a missing one to constant-`false`
    /// (matching the unwired slots in [`SingleAssetStrategy::new`]).
    fn exit(&self, anchor: &Position) -> Box<dyn DynIndicator> {
        self.exit
            .as_ref()
            .map(|s| s.build(anchor))
            .unwrap_or_else(|| dyn_::wrap(Const::<Candle>::new(false)))
    }
}

/// A whole `strategy.yml`: the traded symbol plus its long/short sides.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StrategySpec {
    pub symbol: String,
    #[serde(default)]
    pub long: Option<SideSpec>,
    #[serde(default)]
    pub short: Option<SideSpec>,
}

impl StrategySpec {
    /// Parse a YAML strategy document, resolving `param` placeholders against
    /// `params` first (see [`crate::params`]).
    ///
    /// Two passes: the document is normalized to an untyped [`serde_json::Value`]
    /// (via [`crate::convert::yaml_to_json`], so `!tags` become serde_json's
    /// singleton-map external-tag form), every placeholder node is rewritten to its
    /// resolved value, and only then is the result deserialized into the typed spec
    /// — so a param can stand in for a number, a symbol, or any other field that is
    /// concretely typed here.
    pub fn from_text_with_params(
        text: &str,
        params: &std::collections::HashMap<String, serde_json::Value>,
    ) -> anyhow::Result<Self> {
        let value = crate::input::parse_value(text)?;
        let value = crate::params::substitute(value, params)?;
        Ok(serde_json::from_value(value)?)
    }

    /// Build the live [`DynSingleStrategy`] this spec describes.
    ///
    /// No automatic wrapping — every signal / level is built exactly as the
    /// YAML describes it. If you want to gate an entry on stability, compose
    /// [`Stable`](fugazi::indicators::Stable) explicitly at the signal level:
    /// `enter: !and [<entry>, !stable { source: <source-of-interest> }]`.
    pub fn build(&self) -> DynSingleStrategy {
        let mut strat = SingleAssetStrategy::new(self.symbol.clone());
        // One position per strategy, shared by every `entry`/`peak`/`trough` leaf
        // in the sides' signals and stop levels.
        let anchor = strat.position();
        if let Some(long) = &self.long {
            strat = strat.long_on(
                AsBool::new(long.enter.build(&anchor)),
                AsBool::new(long.exit(&anchor)),
            );
            if let Some(sl) = &long.stop_loss {
                strat = strat.long_stop_loss(AsReal::new(sl.build(&anchor)));
            }
            if let Some(tp) = &long.take_profit {
                strat = strat.long_take_profit(AsReal::new(tp.build(&anchor)));
            }
        }
        if let Some(short) = &self.short {
            strat = strat.short_on(
                AsBool::new(short.enter.build(&anchor)),
                AsBool::new(short.exit(&anchor)),
            );
            if let Some(sl) = &short.stop_loss {
                strat = strat.short_stop_loss(AsReal::new(sl.build(&anchor)));
            }
            if let Some(tp) = &short.take_profit {
                strat = strat.short_take_profit(AsReal::new(tp.build(&anchor)));
            }
        }
        DynSingleStrategy { inner: strat }
    }
}

// ---------------------------------------------------------------------------
// DynSingleStrategy: CLI-owned wrapper around SingleAssetStrategy<String>
// ---------------------------------------------------------------------------

/// The CLI's built-strategy handle. Wraps a [`SingleAssetStrategy<String>`]
/// whose entry/exit signals and protective levels came from runtime-typed
/// [`DynIndicator`]s (bridged into typed [`Signal`](fugazi::Signal) / real
/// levels by the private [`AsBool`] / [`AsReal`] adapters at construction).
///
/// Implements [`Strategy`](fugazi::Strategy) by delegation, so it drops into
/// [`fugazi::backtest::run`] unchanged.
pub struct DynSingleStrategy {
    inner: SingleAssetStrategy<String>,
}

impl Strategy for DynSingleStrategy {
    type Input = Candle;
    type Symbol = String;

    fn update(&mut self, candle: Candle) {
        self.inner.update(candle);
    }
    fn on_fill(&mut self, order: &Order<String>) {
        self.inner.on_fill(order);
    }
    fn trade(&self, wallet: &mut dyn Wallet<String>) {
        self.inner.trade(wallet);
    }
    fn reset(&mut self) {
        self.inner.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dyn_::DynValue as Payload;

    fn bar(close: Real) -> Candle {
        Candle::new(close, close, close, close, 0.0)
    }

    /// Feed a `Box<dyn DynIndicator>` a candle and unwrap the payload as `Real`.
    fn feed_real(source: &mut Box<dyn DynIndicator>, c: Candle) -> Option<Real> {
        match source.update(Payload::Candle(c))? {
            Payload::Real(x) => Some(x),
            other => panic!("expected Real payload, got {other:?}"),
        }
    }

    /// Feed and unwrap as `bool` — for signal-side tests.
    fn feed_bool(source: &mut Box<dyn DynIndicator>, c: Candle) -> Option<bool> {
        match source.update(Payload::Candle(c))? {
            Payload::Bool(b) => Some(b),
            other => panic!("expected Bool payload, got {other:?}"),
        }
    }

    #[test]
    fn builds_an_sma_crossover_signal_that_fires() {
        let yaml = r#"
            !crosses_above
            lhs: !sma { source: close, period: 2 }
            rhs: !sma { source: close, period: 4 }
        "#;
        let spec: SignalSpec = serde_norway::from_str(yaml).unwrap();
        let mut sig = spec.build(&Position::new());
        let mut fired = false;
        for p in [10.0, 9.0, 8.0, 7.0, 8.0, 10.0, 12.0, 14.0, 16.0] {
            fired |= feed_bool(&mut sig, bar(p)).unwrap_or(false);
        }
        assert!(fired, "expected the fast/slow SMA crossover to fire");
    }

    #[test]
    fn probe_yaml_tags_survive_conversion_to_value() {
        let yaml = r#"
            symbol: BTC
            long:
              enter: !crosses_above { lhs: !sma { source: close, period: 3 }, rhs: !sma { period: 8 } }
        "#;
        let value: serde_norway::Value = serde_norway::from_str(yaml).unwrap();
        let json = crate::convert::yaml_to_json(value).unwrap();
        let spec: StrategySpec = serde_json::from_value(json).unwrap();
        assert_eq!(spec.symbol, "BTC");
        assert!(spec.long.is_some());
        let _ = spec.build();
    }

    #[test]
    fn default_source_is_close() {
        let spec: SourceSpec = serde_norway::from_str("!ema { period: 3 }").unwrap();
        let mut ema = spec.build(&Position::new());
        let mut reference = Ema::new(Current::close(), 3);
        for p in [1.0, 2.0, 3.0, 4.0, 5.0] {
            assert_eq!(feed_real(&mut ema, bar(p)), reference.update(bar(p)));
        }
    }

    #[test]
    fn parses_full_strategy_with_long_and_short() {
        let yaml = r#"
            symbol: BTC
            long:
              enter: !crosses_above { lhs: !sma { period: 5 }, rhs: !sma { period: 20 } }
              exit:  !crosses_below { lhs: !sma { period: 5 }, rhs: !sma { period: 20 } }
            short:
              enter: !crosses_below { lhs: !sma { period: 5 }, rhs: !sma { period: 20 } }
              exit:  !crosses_above { lhs: !sma { period: 5 }, rhs: !sma { period: 20 } }
        "#;
        let spec = StrategySpec::from_text_with_params(yaml, &std::collections::HashMap::new())
            .unwrap();
        assert_eq!(spec.symbol, "BTC");
        let _strat = spec.build();
    }

    #[test]
    fn stop_loss_with_entry_source_fires_at_the_level() {
        // Enter on the first bar, with a stop at 90% of entry built from `entry`.
        let yaml = r#"
            symbol: BTC
            long:
              enter: !value true
              stop_loss: !mul { lhs: entry, rhs: !value 0.9 }
        "#;
        let spec =
            StrategySpec::from_text_with_params(yaml, &std::collections::HashMap::new()).unwrap();
        let mut strat = spec.build();
        let mut w = PaperWallet::new(1_000.0);
        for c in [
            Candle::new(100.0, 100.0, 100.0, 100.0, 0.0),
            Candle::new(100.0, 100.0, 100.0, 100.0, 0.0),
            Candle::new(95.0, 96.0, 88.0, 89.0, 0.0),
        ] {
            for fill in w.update("BTC".to_string(), c) {
                strat.on_fill(&fill);
            }
            strat.update(c);
            strat.trade(&mut w);
        }
        assert!(w.positions().next().is_none());
        assert_eq!(w.orders().last().unwrap().price, 90.0);
    }

    #[test]
    fn stable_signal_flips_true_at_source_stable_period() {
        // The new `!stable { signal }` returns a `bool` — false before the
        // inner signal's `stable_period()` samples have arrived, true from
        // that sample onwards. Warm-up = 0 (always emits Some).
        let yaml = r#"
            !stable
            signal: !above { source: !ema { period: 3 }, level: 0 }
        "#;
        let spec: SignalSpec = serde_norway::from_str(yaml).unwrap();
        let mut check = spec.build(&Position::new());
        assert_eq!(check.warm_up_period(), 0);

        // Ema-3 over close: warm_up 1, unstable 10, stable_period 11.
        let inner_stable = Ema::new(Current::close(), 3).above(0.0).stable_period();
        for i in 1..inner_stable {
            assert_eq!(feed_bool(&mut check, bar(i as Real)), Some(false));
        }
        assert_eq!(feed_bool(&mut check, bar(inner_stable as Real)), Some(true));
    }

    #[test]
    fn parses_an_inline_flow_map_strategy() {
        let doc = r#"{"symbol":"ETH","long":{"enter":{"crosses_above":
            {"lhs":{"sma":{"period":5}},"rhs":{"sma":{"period":20}}}}}}"#;
        let spec = StrategySpec::from_text_with_params(doc, &std::collections::HashMap::new())
            .unwrap();
        assert_eq!(spec.symbol, "ETH");
        let _strat = spec.build();
    }

    #[test]
    fn resample_tag_projects_the_field() {
        // `!resample { every: N, inner: close }` emits the resampled close on
        // the Nth base tick, None between.
        let spec: SourceSpec =
            serde_norway::from_str("!resample { every: 4, inner: close }").unwrap();
        let mut built = spec.build(&Position::new());
        for i in 1..=8 {
            let out = feed_real(&mut built, bar(i as Real));
            if i % 4 == 0 {
                assert_eq!(out, Some(i as Real));
            } else {
                assert_eq!(out, None);
            }
        }
    }

    #[test]
    fn latch_tag_holds_the_last_value() {
        // `!latch { source: !resample { every: 3, inner: close } }` — Some on
        // the Nth bar, held on the two between.
        let spec: SourceSpec = serde_norway::from_str(
            "!latch { source: !resample { every: 3, inner: close } }",
        )
        .unwrap();
        let mut built = spec.build(&Position::new());
        assert_eq!(feed_real(&mut built, bar(1.0)), None);
        assert_eq!(feed_real(&mut built, bar(2.0)), None);
        assert_eq!(feed_real(&mut built, bar(3.0)), Some(3.0));
        assert_eq!(feed_real(&mut built, bar(4.0)), Some(3.0));
        assert_eq!(feed_real(&mut built, bar(5.0)), Some(3.0));
        assert_eq!(feed_real(&mut built, bar(6.0)), Some(6.0));
    }

    #[test]
    fn latch_ema_of_resample_matches_reference_htf_ema() {
        // The composition-order regression at the YAML surface: an EMA-3
        // running inside !resample, wrapped in !latch, agrees numerically
        // with Ema(Resample.close, 3) at every boundary.
        let spec: SourceSpec = serde_norway::from_str(
            "!latch { source: !resample { every: 4, inner: !ema { period: 3, source: close } } }",
        )
        .unwrap();
        let mut built = spec.build(&Position::new());
        let mut reference = fugazi::indicators::Latch::new(Ema::new(
            fugazi::indicators::Resample::new(fugazi::indicators::CurrentBar::new(), 4).close(),
            3,
        ));
        for i in 1..=24 {
            let c = bar(100.0 + i as Real * 0.5);
            assert_eq!(feed_real(&mut built, c), reference.update(c));
        }
    }
}
