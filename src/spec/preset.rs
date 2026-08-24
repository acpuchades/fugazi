//! [`StrategyPreset`] / [`StrategyRef`] — YAML sugar for the ready-made
//! single-asset strategies in [`crate::strategies`].
//!
//! A full [`SingleStrategySpec`] spells out every `long`/`short` signal by
//! hand; a **preset** names one of the crate's convenience recipes and its
//! parameters — `!ma_crossover { root: BTC, fast: 3, slow: 8 }` builds the
//! same strategy [`crate::strategies::trend::ma_crossover`] does. Presets
//! reuse the Rust catalogue directly (single source of truth — no re-encoding
//! as a spec tree), so a preset and its Rust twin are identical by construction.
//!
//! [`StrategyRef`] is the "either" type accepted anywhere a single-asset
//! strategy document is: a full spec **or** a preset tag. It backs both the
//! top-level `fugazi run` strategy document and the `strategy:` field of the
//! trailing risk indicators (`!sharpe { strategy: !buy_and_hold { root: X }, … }`).

use crate::types::Symbol;
use std::sync::Arc;

use serde::Deserialize;

use crate::prelude::*;
use crate::strategies::{SingleAssetStrategy, composite, mean_reversion, trend};

use super::meta::Meta;
use super::root::RootSpec;
use super::strategy::{DynSingleStrategy, SingleStrategySpec};

/// The externally-tagged catalogue of ready-made single-asset strategies.
/// Each variant maps one-to-one onto a `crate::strategies` recipe.
///
/// Every variant carries the same optional `meta` as a spelled-out document
/// ([`spec::meta`](crate::spec::meta)) — a preset is a strategy document too,
/// and an external service shouldn't discover that one of the six shapes it can
/// emit is the one that rejects its metadata.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum StrategyPreset {
    /// Go all-in long on the first bar and hold. See
    /// [`SingleAssetStrategy::buy_and_hold`].
    BuyAndHold {
        #[serde(default)]
        root: RootSpec,
        #[serde(default)]
        meta: Option<Meta>,
    },
    /// Always-in SMA fast/slow crossover. See
    /// [`crate::strategies::trend::ma_crossover`].
    MaCrossover {
        #[serde(default)]
        root: RootSpec,
        fast: usize,
        slow: usize,
        #[serde(default)]
        meta: Option<Meta>,
    },
    /// RSI mean-reversion, long/flat: buy when RSI crosses below `oversold`,
    /// exit when it crosses back above `exit`. See
    /// [`crate::strategies::mean_reversion::rsi_reversal`].
    RsiReversal {
        #[serde(default)]
        root: RootSpec,
        period: usize,
        oversold: Real,
        exit: Real,
        #[serde(default)]
        meta: Option<Meta>,
    },
    /// Always-in Donchian channel breakout. See
    /// [`crate::strategies::trend::donchian_breakout`].
    DonchianBreakout {
        #[serde(default)]
        root: RootSpec,
        period: usize,
        #[serde(default)]
        meta: Option<Meta>,
    },
    /// Always-in Keltner channel breakout. See
    /// [`crate::strategies::composite::keltner_breakout`].
    KeltnerBreakout {
        #[serde(default)]
        root: RootSpec,
        ema_period: usize,
        atr_period: usize,
        multiplier: Real,
        #[serde(default)]
        meta: Option<Meta>,
    },
}

/// The lowercase tag names [`StrategyRef`] uses to tell a preset from a full
/// [`SingleStrategySpec`] map. Kept in lock-step with [`StrategyPreset`]'s
/// variants by [`preset_variants_are_listed`](tests::preset_variants_are_listed).
pub(crate) const PRESET_TAGS: &[&str] = &[
    "buy_and_hold",
    "ma_crossover",
    "rsi_reversal",
    "donchian_breakout",
    "keltner_breakout",
];

impl StrategyPreset {
    /// This preset's evaluation root.
    pub fn root(&self) -> &RootSpec {
        match self {
            StrategyPreset::BuyAndHold { root, .. }
            | StrategyPreset::MaCrossover { root, .. }
            | StrategyPreset::RsiReversal { root, .. }
            | StrategyPreset::DonchianBreakout { root, .. }
            | StrategyPreset::KeltnerBreakout { root, .. } => root,
        }
    }

    /// This preset's free-form `meta:`, if the document set one. See
    /// [`spec::meta`](crate::spec::meta).
    pub fn meta(&self) -> Option<&Meta> {
        match self {
            StrategyPreset::BuyAndHold { meta, .. }
            | StrategyPreset::MaCrossover { meta, .. }
            | StrategyPreset::RsiReversal { meta, .. }
            | StrategyPreset::DonchianBreakout { meta, .. }
            | StrategyPreset::KeltnerBreakout { meta, .. } => meta.as_ref(),
        }
    }

    /// Build the live strategy by delegating to the `crate::strategies` recipe.
    ///
    /// Fallible where it used to be infallible: a preset is still constructed
    /// entirely in Rust, but its `root:` is an expression now, so "which one
    /// instrument does this trade" is a question the analysis answers rather
    /// than a field that already holds the answer.
    fn build_strategy(&self) -> Result<SingleAssetStrategy<Symbol>, String> {
        let sym = crate::types::symbol(&self.root().sole_symbol("preset")?);
        Ok(match self {
            StrategyPreset::BuyAndHold { .. } => SingleAssetStrategy::buy_and_hold(sym),
            StrategyPreset::MaCrossover { fast, slow, .. } => {
                trend::ma_crossover(sym, *fast, *slow)
            }
            StrategyPreset::RsiReversal {
                period,
                oversold,
                exit,
                ..
            } => mean_reversion::rsi_reversal(sym, *period, *oversold, *exit),
            StrategyPreset::DonchianBreakout { period, .. } => {
                trend::donchian_breakout(sym, *period)
            }
            StrategyPreset::KeltnerBreakout {
                ema_period,
                atr_period,
                multiplier,
                ..
            } => composite::keltner_breakout(sym, *ema_period, *atr_period, *multiplier),
        })
    }
}

/// Either a full [`SingleStrategySpec`] document or a [`StrategyPreset`] tag.
///
/// Deserialized through a [`serde_norway::Value`] bridge (like
/// [`NodeSpec`](super::NodeSpec)): a value whose tag / single map key is one of
/// `PRESET_TAGS` parses as a preset, anything else as a full spec. Works
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
    /// This strategy's evaluation root.
    pub fn root(&self) -> &RootSpec {
        match self {
            StrategyRef::Spec(s) => &s.root,
            StrategyRef::Preset(p) => p.root(),
        }
    }

    /// The one instrument this strategy trades, or a build error naming why the
    /// root could not say. See [`RootSpec::sole_symbol`].
    pub fn symbol(&self) -> Result<String, String> {
        self.root().sole_symbol(match self {
            StrategyRef::Spec(_) => "single-asset",
            StrategyRef::Preset(_) => "preset",
        })
    }

    /// This document's free-form `meta:`, if it set one — the same key in both
    /// spellings. See [`spec::meta`](crate::spec::meta).
    pub fn meta(&self) -> Option<&Meta> {
        match self {
            StrategyRef::Spec(s) => s.meta.as_ref(),
            StrategyRef::Preset(p) => p.meta(),
        }
    }

    /// Build the live [`DynSingleStrategy`]. `initial_equity` seeds a spec's
    /// [`Book`](crate::indicators::Book) (presets don't read the book, so it's
    /// inert for them); `schema` resolves a spec's `!get` leaves (presets have
    /// none).
    pub fn build(&self, initial_equity: Real, schema: &Arc<Schema>) -> DynSingleStrategy {
        self.try_build(initial_equity, schema)
            .unwrap_or_else(|e| panic!("{e}"))
    }

    /// The fallible twin of [`build`](Self::build). Both arms can fail now —
    /// a preset's recipe is still infallible, but resolving its `root:` to the
    /// single instrument the recipe wants is not.
    pub fn try_build(
        &self,
        initial_equity: Real,
        schema: &Arc<Schema>,
    ) -> Result<DynSingleStrategy, String> {
        match self {
            StrategyRef::Spec(s) => s.try_build(initial_equity, schema),
            StrategyRef::Preset(p) => Ok(DynSingleStrategy::from_single(p.build_strategy()?)),
        }
    }

    /// Load a top-level strategy document (a full spec **or** a preset tag),
    /// splicing `!import`s and resolving `!param`s — the [`StrategyRef`] twin of
    /// [`SingleStrategySpec::from_text_with_params_in`].
    pub fn from_text_with_params_in(
        text: &str,
        params: &std::collections::HashMap<String, serde_json::Value>,
        base: &std::path::Path,
        root: &std::path::Path,
        label: &str,
    ) -> anyhow::Result<Self> {
        use anyhow::Context;
        let value = super::load_document(
            text,
            params,
            base,
            root,
            label,
            super::input::StrategyKind::Single,
        )?;
        serde_json::from_value(value).with_context(|| format!("building strategy from {label}"))
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
        // preset — exactly the NodeSpec pattern.
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
                crate::spec::undefined::from_value(tagged).map_err(|e| e.to_string())?;
            Ok(StrategyRef::Preset(p))
        } else {
            let s: SingleStrategySpec =
                crate::spec::undefined::from_value(v).map_err(|e| e.to_string())?;
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
            "!buy_and_hold { root: X }",
            "!ma_crossover { root: X, fast: 3, slow: 8 }",
            "!rsi_reversal { root: X, period: 14, oversold: 30, exit: 50 }",
            "!donchian_breakout { root: X, period: 20 }",
            "!keltner_breakout { root: X, ema_period: 20, atr_period: 10, multiplier: 2.0 }",
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
            "{ root: X, long: { enter: !gt { lhs: !close, rhs: !value 0.0 } } }",
        )
        .unwrap();
        assert!(matches!(r, StrategyRef::Spec(_)));
        assert_eq!(r.symbol().unwrap(), "X");
    }

    #[test]
    fn preset_symbol_reads_through() {
        let r: StrategyRef =
            serde_norway::from_str("!ma_crossover { root: BTC, fast: 3, slow: 8 }").unwrap();
        assert_eq!(r.symbol().unwrap(), "BTC");
        let _ = r.build(1_000.0, &Schema::empty());
    }
}
