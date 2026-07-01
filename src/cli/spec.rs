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
    DEFAULT_EPSILON, Dmi, DmiValue, Donchian, DonchianValue, Ema, Hma, Keltner, KeltnerValue, Macd,
    MacdValue, Mfi, Obv, Position, Rma, Rsi, Sar, Sma, StdDev, StochRsi, Stochastic, TrueRange,
    Value, Vwap, WilliamsR, Wma,
};
use fugazi::indicators::compare;
use fugazi::indicators::logic::Const;
use fugazi::prelude::*;
use fugazi::strategies::SingleAssetStrategy;

use crate::dynd::{DynSignal, DynValue};

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
}

impl SourceSpec {
    /// Construct the live, type-erased source this spec describes. `anchor` is
    /// the owning strategy's [`Position`], shared by any `entry` / `peak` /
    /// `trough` leaves in the tree.
    pub fn build(&self, anchor: &Position) -> DynValue {
        use SourceSpec::*;
        match self {
            Close => DynValue::new(Current::close()),
            High => DynValue::new(Current::high()),
            Low => DynValue::new(Current::low()),
            Open => DynValue::new(Current::open()),
            Volume => DynValue::new(Current::volume()),
            Typical => DynValue::new(Current::typical()),
            Median => DynValue::new(Current::median()),
            Value(x) => DynValue::new(self::Value::<Candle>::new(*x)),
            Entry => DynValue::new(anchor.entry()),
            Peak => DynValue::new(anchor.peak()),
            Trough => DynValue::new(anchor.trough()),

            Ema { source, period } => DynValue::new(self::Ema::new(source.build(anchor), *period)),
            Sma { source, period } => DynValue::new(self::Sma::new(source.build(anchor), *period)),
            Rma { source, period } => DynValue::new(self::Rma::new(source.build(anchor), *period)),
            Wma { source, period } => DynValue::new(self::Wma::new(source.build(anchor), *period)),
            Hma { source, period } => DynValue::new(self::Hma::new(source.build(anchor), *period)),
            Rsi { source, period } => DynValue::new(self::Rsi::new(source.build(anchor), *period)),
            StdDev { source, period } => DynValue::new(self::StdDev::new(source.build(anchor), *period)),
            Cci { source, period } => DynValue::new(self::Cci::new(source.build(anchor), *period)),
            Stochastic { source, period } => {
                DynValue::new(self::Stochastic::new(source.build(anchor), *period))
            }
            StochRsi {
                source,
                rsi_period,
                stoch_period,
            } => DynValue::new(self::StochRsi::new(
                self::Rsi::new(source.build(anchor), *rsi_period),
                *stoch_period,
            )),

            MacdLine {
                source,
                fast,
                slow,
                signal,
            } => DynValue::new(Component::new(
                Macd::new(source.build(anchor), *fast, *slow, *signal),
                |v: MacdValue| v.macd,
            )),
            MacdSignal {
                source,
                fast,
                slow,
                signal,
            } => DynValue::new(Component::new(
                Macd::new(source.build(anchor), *fast, *slow, *signal),
                |v: MacdValue| v.signal,
            )),
            MacdHistogram {
                source,
                fast,
                slow,
                signal,
            } => DynValue::new(Component::new(
                Macd::new(source.build(anchor), *fast, *slow, *signal),
                |v: MacdValue| v.histogram,
            )),

            BbUpper { source, period, k } => DynValue::new(Component::new(
                Bollinger::new(source.build(anchor), *period, *k),
                |v: BollingerValue| v.upper,
            )),
            BbMiddle { source, period, k } => DynValue::new(Component::new(
                Bollinger::new(source.build(anchor), *period, *k),
                |v: BollingerValue| v.middle,
            )),
            BbLower { source, period, k } => DynValue::new(Component::new(
                Bollinger::new(source.build(anchor), *period, *k),
                |v: BollingerValue| v.lower,
            )),

            KeltnerUpper {
                source,
                ema_period,
                atr_period,
                multiplier,
            } => DynValue::new(Component::new(
                Keltner::new(source.build(anchor), *ema_period, *atr_period, *multiplier),
                |v: KeltnerValue| v.upper,
            )),
            KeltnerMiddle {
                source,
                ema_period,
                atr_period,
                multiplier,
            } => DynValue::new(Component::new(
                Keltner::new(source.build(anchor), *ema_period, *atr_period, *multiplier),
                |v: KeltnerValue| v.middle,
            )),
            KeltnerLower {
                source,
                ema_period,
                atr_period,
                multiplier,
            } => DynValue::new(Component::new(
                Keltner::new(source.build(anchor), *ema_period, *atr_period, *multiplier),
                |v: KeltnerValue| v.lower,
            )),

            DonchianUpper { high, low, period } => DynValue::new(Component::new(
                Donchian::new(high.build(anchor), low.build(anchor), *period),
                |v: DonchianValue| v.upper,
            )),
            DonchianMiddle { high, low, period } => DynValue::new(Component::new(
                Donchian::new(high.build(anchor), low.build(anchor), *period),
                |v: DonchianValue| v.middle,
            )),
            DonchianLower { high, low, period } => DynValue::new(Component::new(
                Donchian::new(high.build(anchor), low.build(anchor), *period),
                |v: DonchianValue| v.lower,
            )),

            Adx { period } => {
                DynValue::new(Component::new(self::Adx::new(*period), |v: AdxValue| v.adx))
            }
            PlusDi { period } => DynValue::new(Component::new(self::Adx::new(*period), |v: AdxValue| {
                v.plus_di
            })),
            MinusDi { period } => DynValue::new(Component::new(self::Adx::new(*period), |v: AdxValue| {
                v.minus_di
            })),
            DmiPlusDi { period } => DynValue::new(Component::new(self::Dmi::new(*period), |v: DmiValue| {
                v.plus_di
            })),
            DmiMinusDi { period } => DynValue::new(Component::new(self::Dmi::new(*period), |v: DmiValue| {
                v.minus_di
            })),

            AroonUp { period } => DynValue::new(Component::new(self::Aroon::new(*period), |v: AroonValue| {
                v.up
            })),
            AroonDown { period } => DynValue::new(Component::new(self::Aroon::new(*period), |v: AroonValue| {
                v.down
            })),
            AroonOscillator { period } => DynValue::new(Component::new(
                self::Aroon::new(*period),
                |v: AroonValue| v.oscillator,
            )),

            Atr { period } => DynValue::new(self::Atr::new(*period)),
            Mfi { period } => DynValue::new(self::Mfi::new(*period)),
            WilliamsR { period } => DynValue::new(self::WilliamsR::new(*period)),
            Obv => DynValue::new(self::Obv::new()),
            Vwap => DynValue::new(self::Vwap::new()),
            Ad => DynValue::new(self::Ad::new()),
            TrueRange => DynValue::new(self::TrueRange::new()),
            Sar { step, max } => DynValue::new(self::Sar::new(*step, *max)),

            Add { lhs, rhs } => DynValue::new(lhs.build(anchor).add(rhs.build(anchor))),
            Sub { lhs, rhs } => DynValue::new(lhs.build(anchor).sub(rhs.build(anchor))),
            Mul { lhs, rhs } => DynValue::new(lhs.build(anchor).mul(rhs.build(anchor))),
            Div { lhs, rhs } => DynValue::new(lhs.build(anchor).div(rhs.build(anchor))),
            Lag { source, periods } => DynValue::new(source.build(anchor).lag(*periods)),
            Diff { source, periods } => DynValue::new(source.build(anchor).diff(*periods)),
            Ratio { source, periods } => DynValue::new(source.build(anchor).ratio(*periods)),
            Roc { source, periods } => DynValue::new(source.build(anchor).roc(*periods)),
            RollingMax { source, period } => DynValue::new(source.build(anchor).rolling_max(*period)),
            RollingMin { source, period } => DynValue::new(source.build(anchor).rolling_min(*period)),
        }
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
    /// A constant boolean leaf. Spelled `!value` like [`SourceSpec::Value`] —
    /// one tag for "a literal", typed by position (bool here, number there).
    Value(bool),
}

/// Resolve an optional tolerance to its concrete value.
fn eps(epsilon: &Option<Real>) -> Real {
    epsilon.unwrap_or(DEFAULT_EPSILON)
}

impl SignalSpec {
    /// Construct the live, type-erased signal this spec describes. `anchor` is
    /// threaded to any `entry` / `peak` / `trough` source leaf in the tree.
    pub fn build(&self, anchor: &Position) -> DynSignal {
        use SignalSpec::*;
        match self {
            Gt { lhs, rhs, epsilon } => {
                DynSignal::new(compare::Gt::with_epsilon(lhs.build(anchor), rhs.build(anchor), eps(epsilon)))
            }
            Lt { lhs, rhs, epsilon } => {
                DynSignal::new(compare::Lt::with_epsilon(lhs.build(anchor), rhs.build(anchor), eps(epsilon)))
            }
            Ge { lhs, rhs, epsilon } => {
                DynSignal::new(compare::Ge::with_epsilon(lhs.build(anchor), rhs.build(anchor), eps(epsilon)))
            }
            Le { lhs, rhs, epsilon } => {
                DynSignal::new(compare::Le::with_epsilon(lhs.build(anchor), rhs.build(anchor), eps(epsilon)))
            }
            Eq { lhs, rhs, epsilon } => {
                DynSignal::new(compare::Eq::with_epsilon(lhs.build(anchor), rhs.build(anchor), eps(epsilon)))
            }
            Ne { lhs, rhs, epsilon } => {
                DynSignal::new(compare::Ne::with_epsilon(lhs.build(anchor), rhs.build(anchor), eps(epsilon)))
            }
            Above { source, level } => DynSignal::new(source.build(anchor).above(*level)),
            Below { source, level } => DynSignal::new(source.build(anchor).below(*level)),

            // A crossover clones its operands; the boxes here are not `Clone`, so
            // we rebuild each operand from the spec to get two independent
            // instances and assemble the expansion by hand.
            CrossesAbove { lhs, rhs } => {
                let cmp = || lhs.build(anchor).gt(rhs.build(anchor));
                DynSignal::new(cmp().and(cmp().changed()))
            }
            CrossesBelow { lhs, rhs } => {
                let cmp = || lhs.build(anchor).lt(rhs.build(anchor));
                DynSignal::new(cmp().and(cmp().changed()))
            }

            And { lhs, rhs } => DynSignal::new(lhs.build(anchor).and(rhs.build(anchor))),
            Or { lhs, rhs } => DynSignal::new(lhs.build(anchor).or(rhs.build(anchor))),
            Xor { lhs, rhs } => DynSignal::new(lhs.build(anchor).xor(rhs.build(anchor))),
            All(specs) => specs
                .iter()
                .map(|s| s.build(anchor))
                .reduce(|acc, s| DynSignal::new(acc.and(s)))
                .unwrap_or_else(|| DynSignal::new(self::Const::<Candle>::new(true))),
            Any(specs) => specs
                .iter()
                .map(|s| s.build(anchor))
                .reduce(|acc, s| DynSignal::new(acc.or(s)))
                .unwrap_or_else(|| DynSignal::new(self::Const::<Candle>::new(false))),
            Not(inner) => DynSignal::new(inner.build(anchor).not()),
            Changed(inner) => DynSignal::new(inner.build(anchor).changed()),
            Value(b) => DynSignal::new(self::Const::<Candle>::new(*b)),
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
    fn exit(&self, anchor: &Position) -> DynSignal {
        self.exit
            .as_ref()
            .map(|s| s.build(anchor))
            .unwrap_or_else(|| DynSignal::new(Const::<Candle>::new(false)))
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

    /// Build the live [`SingleAssetStrategy`] this spec describes.
    pub fn build(&self) -> SingleAssetStrategy<String> {
        let mut strat = SingleAssetStrategy::new(self.symbol.clone());
        // One position per strategy, shared by every `entry`/`peak`/`trough` leaf
        // in the sides' signals and stop levels.
        let anchor = strat.position();
        if let Some(long) = &self.long {
            strat = strat.long_on(long.enter.build(&anchor), long.exit(&anchor));
            if let Some(sl) = &long.stop_loss {
                strat = strat.long_stop_loss(sl.build(&anchor));
            }
            if let Some(tp) = &long.take_profit {
                strat = strat.long_take_profit(tp.build(&anchor));
            }
        }
        if let Some(short) = &self.short {
            strat = strat.short_on(short.enter.build(&anchor), short.exit(&anchor));
            if let Some(sl) = &short.stop_loss {
                strat = strat.short_stop_loss(sl.build(&anchor));
            }
            if let Some(tp) = &short.take_profit {
                strat = strat.short_take_profit(tp.build(&anchor));
            }
        }
        strat
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar(close: Real) -> Candle {
        Candle::new(close, close, close, close, 0.0)
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
        // A dip then a rally drives the fast SMA up through the slow one.
        let mut fired = false;
        for p in [10.0, 9.0, 8.0, 7.0, 8.0, 10.0, 12.0, 14.0, 16.0] {
            sig.update(bar(p));
            fired |= sig.is_true();
        }
        assert!(fired, "expected the fast/slow SMA crossover to fire");
    }

    #[test]
    fn probe_yaml_tags_survive_conversion_to_value() {
        // A `!tag` YAML doc, converted to a serde_json::Value (tags → singleton
        // maps) and deserialized, must yield the spec the tags describe.
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
        // `period` only — source should default to the close.
        let spec: SourceSpec = serde_norway::from_str("!ema { period: 3 }").unwrap();
        let mut ema = spec.build(&Position::new());
        let mut reference = Ema::new(Current::close(), 3);
        for p in [1.0, 2.0, 3.0, 4.0, 5.0] {
            assert_eq!(ema.update(bar(p)), reference.update(bar(p)));
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
        // Bar 1 signals; the entry fills at bar 2's open (100), anchoring the stop
        // at 90; bar 3 trades down through 90 (low 88), opening above it.
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
        assert!(w.is_flat());
        assert_eq!(w.orders().last().unwrap().price, 90.0);
    }

    #[test]
    fn parses_an_inline_flow_map_strategy() {
        // Flow-style YAML (the map form, JSON being a subset) parses through the
        // same single path — externally-tagged variants spelled as singleton maps.
        let doc = r#"{"symbol":"ETH","long":{"enter":{"crosses_above":
            {"lhs":{"sma":{"period":5}},"rhs":{"sma":{"period":20}}}}}}"#;
        let spec = StrategySpec::from_text_with_params(doc, &std::collections::HashMap::new())
            .unwrap();
        assert_eq!(spec.symbol, "ETH");
        let _strat = spec.build();
    }
}
