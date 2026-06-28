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
    MacdValue, Mfi, Obv, Rma, Rsi, Sar, Sma, StdDev, StochRsi, Stochastic, TrueRange, Value, Vwap,
    WilliamsR, Wma,
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
    /// Construct the live, type-erased source this spec describes.
    pub fn build(&self) -> DynValue {
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

            Ema { source, period } => DynValue::new(self::Ema::new(source.build(), *period)),
            Sma { source, period } => DynValue::new(self::Sma::new(source.build(), *period)),
            Rma { source, period } => DynValue::new(self::Rma::new(source.build(), *period)),
            Wma { source, period } => DynValue::new(self::Wma::new(source.build(), *period)),
            Hma { source, period } => DynValue::new(self::Hma::new(source.build(), *period)),
            Rsi { source, period } => DynValue::new(self::Rsi::new(source.build(), *period)),
            StdDev { source, period } => DynValue::new(self::StdDev::new(source.build(), *period)),
            Cci { source, period } => DynValue::new(self::Cci::new(source.build(), *period)),
            Stochastic { source, period } => {
                DynValue::new(self::Stochastic::new(source.build(), *period))
            }
            StochRsi {
                source,
                rsi_period,
                stoch_period,
            } => DynValue::new(self::StochRsi::new(
                self::Rsi::new(source.build(), *rsi_period),
                *stoch_period,
            )),

            MacdLine {
                source,
                fast,
                slow,
                signal,
            } => DynValue::new(Component::new(
                Macd::new(source.build(), *fast, *slow, *signal),
                |v: MacdValue| v.macd,
            )),
            MacdSignal {
                source,
                fast,
                slow,
                signal,
            } => DynValue::new(Component::new(
                Macd::new(source.build(), *fast, *slow, *signal),
                |v: MacdValue| v.signal,
            )),
            MacdHistogram {
                source,
                fast,
                slow,
                signal,
            } => DynValue::new(Component::new(
                Macd::new(source.build(), *fast, *slow, *signal),
                |v: MacdValue| v.histogram,
            )),

            BbUpper { source, period, k } => DynValue::new(Component::new(
                Bollinger::new(source.build(), *period, *k),
                |v: BollingerValue| v.upper,
            )),
            BbMiddle { source, period, k } => DynValue::new(Component::new(
                Bollinger::new(source.build(), *period, *k),
                |v: BollingerValue| v.middle,
            )),
            BbLower { source, period, k } => DynValue::new(Component::new(
                Bollinger::new(source.build(), *period, *k),
                |v: BollingerValue| v.lower,
            )),

            KeltnerUpper {
                source,
                ema_period,
                atr_period,
                multiplier,
            } => DynValue::new(Component::new(
                Keltner::new(source.build(), *ema_period, *atr_period, *multiplier),
                |v: KeltnerValue| v.upper,
            )),
            KeltnerMiddle {
                source,
                ema_period,
                atr_period,
                multiplier,
            } => DynValue::new(Component::new(
                Keltner::new(source.build(), *ema_period, *atr_period, *multiplier),
                |v: KeltnerValue| v.middle,
            )),
            KeltnerLower {
                source,
                ema_period,
                atr_period,
                multiplier,
            } => DynValue::new(Component::new(
                Keltner::new(source.build(), *ema_period, *atr_period, *multiplier),
                |v: KeltnerValue| v.lower,
            )),

            DonchianUpper { high, low, period } => DynValue::new(Component::new(
                Donchian::new(high.build(), low.build(), *period),
                |v: DonchianValue| v.upper,
            )),
            DonchianMiddle { high, low, period } => DynValue::new(Component::new(
                Donchian::new(high.build(), low.build(), *period),
                |v: DonchianValue| v.middle,
            )),
            DonchianLower { high, low, period } => DynValue::new(Component::new(
                Donchian::new(high.build(), low.build(), *period),
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

            Add { lhs, rhs } => DynValue::new(lhs.build().add(rhs.build())),
            Sub { lhs, rhs } => DynValue::new(lhs.build().sub(rhs.build())),
            Mul { lhs, rhs } => DynValue::new(lhs.build().mul(rhs.build())),
            Div { lhs, rhs } => DynValue::new(lhs.build().div(rhs.build())),
            Lag { source, periods } => DynValue::new(source.build().lag(*periods)),
            Diff { source, periods } => DynValue::new(source.build().diff(*periods)),
            Ratio { source, periods } => DynValue::new(source.build().ratio(*periods)),
            Roc { source, periods } => DynValue::new(source.build().roc(*periods)),
            RollingMax { source, period } => DynValue::new(source.build().rolling_max(*period)),
            RollingMin { source, period } => DynValue::new(source.build().rolling_min(*period)),
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
    /// Construct the live, type-erased signal this spec describes.
    pub fn build(&self) -> DynSignal {
        use SignalSpec::*;
        match self {
            Gt { lhs, rhs, epsilon } => {
                DynSignal::new(compare::Gt::with_epsilon(lhs.build(), rhs.build(), eps(epsilon)))
            }
            Lt { lhs, rhs, epsilon } => {
                DynSignal::new(compare::Lt::with_epsilon(lhs.build(), rhs.build(), eps(epsilon)))
            }
            Ge { lhs, rhs, epsilon } => {
                DynSignal::new(compare::Ge::with_epsilon(lhs.build(), rhs.build(), eps(epsilon)))
            }
            Le { lhs, rhs, epsilon } => {
                DynSignal::new(compare::Le::with_epsilon(lhs.build(), rhs.build(), eps(epsilon)))
            }
            Eq { lhs, rhs, epsilon } => {
                DynSignal::new(compare::Eq::with_epsilon(lhs.build(), rhs.build(), eps(epsilon)))
            }
            Ne { lhs, rhs, epsilon } => {
                DynSignal::new(compare::Ne::with_epsilon(lhs.build(), rhs.build(), eps(epsilon)))
            }
            Above { source, level } => DynSignal::new(source.build().above(*level)),
            Below { source, level } => DynSignal::new(source.build().below(*level)),

            // A crossover clones its operands; the boxes here are not `Clone`, so
            // we rebuild each operand from the spec to get two independent
            // instances and assemble the expansion by hand.
            CrossesAbove { lhs, rhs } => {
                let cmp = || lhs.build().gt(rhs.build());
                DynSignal::new(cmp().and(cmp().changed()))
            }
            CrossesBelow { lhs, rhs } => {
                let cmp = || lhs.build().lt(rhs.build());
                DynSignal::new(cmp().and(cmp().changed()))
            }

            And { lhs, rhs } => DynSignal::new(lhs.build().and(rhs.build())),
            Or { lhs, rhs } => DynSignal::new(lhs.build().or(rhs.build())),
            Xor { lhs, rhs } => DynSignal::new(lhs.build().xor(rhs.build())),
            All(specs) => specs
                .iter()
                .map(SignalSpec::build)
                .reduce(|acc, s| DynSignal::new(acc.and(s)))
                .unwrap_or_else(|| DynSignal::new(self::Const::<Candle>::new(true))),
            Any(specs) => specs
                .iter()
                .map(SignalSpec::build)
                .reduce(|acc, s| DynSignal::new(acc.or(s)))
                .unwrap_or_else(|| DynSignal::new(self::Const::<Candle>::new(false))),
            Not(inner) => DynSignal::new(inner.build().not()),
            Changed(inner) => DynSignal::new(inner.build().changed()),
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
}

impl SideSpec {
    /// Build this side's exit signal, defaulting a missing one to constant-`false`
    /// (matching the unwired slots in [`SingleAssetStrategy::new`]).
    fn exit(&self) -> DynSignal {
        self.exit
            .as_ref()
            .map(SignalSpec::build)
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
    /// Parse a strategy document (YAML or JSON), resolving `param` placeholders
    /// against `params` first (see [`crate::params`]).
    ///
    /// Two passes: the document is normalized to an untyped [`serde_json::Value`]
    /// (YAML via [`crate::convert::yaml_to_json`], JSON straight in), every
    /// placeholder node is rewritten to its resolved value, and only then is the
    /// result deserialized into the typed spec — so a param can stand in for a
    /// number, a symbol, or any other field that is concretely typed here.
    pub fn from_text_with_params(
        text: &str,
        format: crate::input::Format,
        params: &std::collections::HashMap<String, serde_json::Value>,
    ) -> anyhow::Result<Self> {
        let value = match format {
            crate::input::Format::Json => serde_json::from_str(text)?,
            crate::input::Format::Yaml => {
                crate::convert::yaml_to_json(serde_norway::from_str(text)?)?
            }
        };
        let value = crate::params::substitute(value, params)?;
        Ok(serde_json::from_value(value)?)
    }

    /// Build the live [`SingleAssetStrategy`] this spec describes.
    pub fn build(&self) -> SingleAssetStrategy<String> {
        let mut strat = SingleAssetStrategy::new(self.symbol.clone());
        if let Some(long) = &self.long {
            strat = strat.long_on(long.enter.build(), long.exit());
        }
        if let Some(short) = &self.short {
            strat = strat.short_on(short.enter.build(), short.exit());
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
        let mut sig = spec.build();
        // A dip then a rally drives the fast SMA up through the slow one.
        let mut fired = false;
        for p in [10.0, 9.0, 8.0, 7.0, 8.0, 10.0, 12.0, 14.0, 16.0] {
            sig.update(bar(p));
            fired |= sig.is_true();
        }
        assert!(fired, "expected the fast/slow SMA crossover to fire");
    }

    #[test]
    fn probe_yaml_tags_survive_conversion_to_json() {
        // A `!tag` YAML doc, converted to JSON (tags → singleton maps) and
        // deserialized via serde_json, must yield the same spec as YAML would.
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
        let mut ema = spec.build();
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
        let spec = StrategySpec::from_text_with_params(
            yaml,
            crate::input::Format::Yaml,
            &std::collections::HashMap::new(),
        )
        .unwrap();
        assert_eq!(spec.symbol, "BTC");
        let _strat = spec.build();
    }

    #[test]
    fn parses_an_inline_json_strategy() {
        let json = r#"{"symbol":"ETH","long":{"enter":{"crosses_above":
            {"lhs":{"sma":{"period":5}},"rhs":{"sma":{"period":20}}}}}}"#;
        let spec = StrategySpec::from_text_with_params(
            json,
            crate::input::Format::Json,
            &std::collections::HashMap::new(),
        )
        .unwrap();
        assert_eq!(spec.symbol, "ETH");
        let _strat = spec.build();
    }
}
