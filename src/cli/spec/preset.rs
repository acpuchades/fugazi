//! [`StrategyPreset`] / [`StrategyRef`] — YAML sugar for the ready-made
//! single-asset strategies in [`fugazi::strategies`].
//!
//! A full [`SingleStrategySpec`] spells out every `long`/`short` signal by
//! hand; a **preset** names one of the crate's convenience recipes and its
//! parameters — `!ma_crossover { symbol: BTC, fast: 3, slow: 8 }` builds the
//! same strategy [`fugazi::strategies::trend::ma_crossover`] does. Presets
//! reuse the Rust catalogue directly (single source of truth — no re-encoding
//! as a spec tree), so a preset and its Rust twin are identical by construction.
//!
//! [`StrategyRef`] is the "either" type accepted anywhere a single-asset
//! strategy document is: a full spec **or** a preset tag. It backs both the
//! top-level `fugazi run` strategy document and the `strategy:` field of the
//! trailing risk indicators (`!sharpe { strategy: !buy_and_hold { symbol: X }, … }`).

use std::sync::Arc;

use serde::Deserialize;

use fugazi::prelude::*;
use fugazi::strategies::{SingleAssetStrategy, composite, mean_reversion, trend};

use super::strategy::{DynSingleStrategy, SingleStrategySpec};

/// The externally-tagged catalogue of ready-made single-asset strategies.
/// Each variant maps one-to-one onto a `fugazi::strategies` recipe.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum StrategyPreset {
    /// Go all-in long on the first bar and hold. See
    /// [`SingleAssetStrategy::buy_and_hold`].
    BuyAndHold { symbol: String },
    /// Always-in SMA fast/slow crossover. See
    /// [`fugazi::strategies::trend::ma_crossover`].
    MaCrossover {
        symbol: String,
        fast: usize,
        slow: usize,
    },
    /// RSI mean-reversion, long/flat: buy when RSI crosses below `oversold`,
    /// exit when it crosses back above `exit`. See
    /// [`fugazi::strategies::mean_reversion::rsi_reversal`].
    RsiReversal {
        symbol: String,
        period: usize,
        oversold: Real,
        exit: Real,
    },
    /// Always-in Donchian channel breakout. See
    /// [`fugazi::strategies::trend::donchian_breakout`].
    DonchianBreakout { symbol: String, period: usize },
    /// Always-in Keltner channel breakout. See
    /// [`fugazi::strategies::composite::keltner_breakout`].
    KeltnerBreakout {
        symbol: String,
        ema_period: usize,
        atr_period: usize,
        multiplier: Real,
    },
}

/// The lowercase tag names [`StrategyRef`] uses to tell a preset from a full
/// [`SingleStrategySpec`] map. Kept in lock-step with [`StrategyPreset`]'s
/// variants by [`preset_variants_are_listed`](tests::preset_variants_are_listed).
const PRESET_TAGS: &[&str] = &[
    "buy_and_hold",
    "ma_crossover",
    "rsi_reversal",
    "donchian_breakout",
    "keltner_breakout",
];

impl StrategyPreset {
    /// The instrument this preset trades.
    pub fn symbol(&self) -> &str {
        match self {
            StrategyPreset::BuyAndHold { symbol }
            | StrategyPreset::MaCrossover { symbol, .. }
            | StrategyPreset::RsiReversal { symbol, .. }
            | StrategyPreset::DonchianBreakout { symbol, .. }
            | StrategyPreset::KeltnerBreakout { symbol, .. } => symbol,
        }
    }

    /// Build the live strategy by delegating to the `fugazi::strategies` recipe.
    fn build_strategy(&self) -> SingleAssetStrategy<String> {
        match self {
            StrategyPreset::BuyAndHold { symbol } => {
                SingleAssetStrategy::buy_and_hold(symbol.clone())
            }
            StrategyPreset::MaCrossover { symbol, fast, slow } => {
                trend::ma_crossover(symbol.clone(), *fast, *slow)
            }
            StrategyPreset::RsiReversal {
                symbol,
                period,
                oversold,
                exit,
            } => mean_reversion::rsi_reversal(symbol.clone(), *period, *oversold, *exit),
            StrategyPreset::DonchianBreakout { symbol, period } => {
                trend::donchian_breakout(symbol.clone(), *period)
            }
            StrategyPreset::KeltnerBreakout {
                symbol,
                ema_period,
                atr_period,
                multiplier,
            } => composite::keltner_breakout(symbol.clone(), *ema_period, *atr_period, *multiplier),
        }
    }
}

/// Either a full [`SingleStrategySpec`] document or a [`StrategyPreset`] tag.
///
/// Deserialized through a [`serde_norway::Value`] bridge (like
/// [`ExprSpec`](super::ExprSpec)): a value whose tag / single map key is one of
/// [`PRESET_TAGS`] parses as a preset, anything else as a full spec. Works
/// through both the YAML (`serde_norway`) path — the trailing indicators'
/// `strategy:` field — and the `serde_json` load path — a top-level `fugazi run`
/// document.
#[derive(Debug, Clone, Deserialize)]
#[serde(try_from = "serde_norway::Value")]
pub enum StrategyRef {
    Spec(Box<SingleStrategySpec>),
    Preset(StrategyPreset),
}

impl StrategyRef {
    /// The instrument this strategy trades.
    pub fn symbol(&self) -> &str {
        match self {
            StrategyRef::Spec(s) => &s.symbol,
            StrategyRef::Preset(p) => p.symbol(),
        }
    }

    /// Build the live [`DynSingleStrategy`]. `initial_equity` seeds a spec's
    /// [`Book`](fugazi::indicators::Book) (presets don't read the book, so it's
    /// inert for them); `schema` resolves a spec's `!get` leaves (presets have
    /// none).
    pub fn build(&self, initial_equity: Real, schema: &Arc<Schema>) -> DynSingleStrategy {
        match self {
            StrategyRef::Spec(s) => s.build(initial_equity, schema),
            StrategyRef::Preset(p) => DynSingleStrategy::from_single(p.build_strategy()),
        }
    }

    /// Load a top-level strategy document (a full spec **or** a preset tag),
    /// splicing `!import`s and resolving `!param`s — the [`StrategyRef`] twin of
    /// [`SingleStrategySpec::from_text_with_params_in`].
    pub fn from_text_with_params_in(
        text: &str,
        params: &std::collections::HashMap<String, serde_json::Value>,
        base: &std::path::Path,
        label: &str,
    ) -> anyhow::Result<Self> {
        use anyhow::Context;
        let value = super::load_value(text, params, base, label)?;
        serde_json::from_value(value)
            .with_context(|| format!("building strategy from {label}"))
    }
}

impl TryFrom<serde_norway::Value> for StrategyRef {
    type Error = String;

    fn try_from(v: serde_norway::Value) -> Result<Self, Self::Error> {
        use serde_norway::Value;
        use serde_norway::value::{Tag, TaggedValue};

        let is_preset = |name: &str| PRESET_TAGS.contains(&name);

        // A preset arrives either as a YAML `!tag { … }` (Value::Tagged) or,
        // on the serde_json load path, as a single-key `{ tag: { … } }` mapping.
        // serde_norway only routes an *enum* through Value::Tagged, so a
        // single-key mapping is normalised to Tagged before deserializing the
        // preset — exactly the ExprSpec pattern.
        let tagged_preset: Option<Value> = match &v {
            Value::Tagged(t) => {
                let name = t.tag.to_string();
                let name = name.strip_prefix('!').unwrap_or(&name);
                is_preset(name).then(|| v.clone())
            }
            Value::Mapping(m) if m.len() == 1 => match m.iter().next() {
                Some((Value::String(k), val)) if is_preset(k) => {
                    Some(Value::Tagged(Box::new(TaggedValue {
                        tag: Tag::new(k.clone()),
                        value: val.clone(),
                    })))
                }
                _ => None,
            },
            _ => None,
        };

        if let Some(tagged) = tagged_preset {
            let p: StrategyPreset =
                serde_norway::from_value(tagged).map_err(|e| e.to_string())?;
            Ok(StrategyRef::Preset(p))
        } else {
            let s: SingleStrategySpec =
                serde_norway::from_value(v).map_err(|e| e.to_string())?;
            Ok(StrategyRef::Spec(Box::new(s)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every [`StrategyPreset`] variant's snake_case tag is in [`PRESET_TAGS`],
    /// so [`StrategyRef`]'s preset-vs-spec detection can't silently miss one.
    #[test]
    fn preset_variants_are_listed() {
        // A representative value per variant; if a variant is added without a
        // PRESET_TAGS entry, its `!tag` form parses as a Spec and fails here.
        for text in [
            "!buy_and_hold { symbol: X }",
            "!ma_crossover { symbol: X, fast: 3, slow: 8 }",
            "!rsi_reversal { symbol: X, period: 14, oversold: 30, exit: 50 }",
            "!donchian_breakout { symbol: X, period: 20 }",
            "!keltner_breakout { symbol: X, ema_period: 20, atr_period: 10, multiplier: 2.0 }",
        ] {
            let r: StrategyRef = serde_norway::from_str(text).unwrap();
            assert!(
                matches!(r, StrategyRef::Preset(_)),
                "`{text}` did not parse as a preset"
            );
        }
    }

    #[test]
    fn a_full_spec_map_parses_as_spec_not_preset() {
        let r: StrategyRef = serde_norway::from_str(
            "{ symbol: X, long: { enter: !gt { lhs: !close, rhs: !value 0.0 } } }",
        )
        .unwrap();
        assert!(matches!(r, StrategyRef::Spec(_)));
        assert_eq!(r.symbol(), "X");
    }

    #[test]
    fn preset_symbol_reads_through() {
        let r: StrategyRef =
            serde_norway::from_str("!ma_crossover { symbol: BTC, fast: 3, slow: 8 }").unwrap();
        assert_eq!(r.symbol(), "BTC");
        let _ = r.build(1_000.0, &Schema::empty());
    }
}
