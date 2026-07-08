//! YAML-deserializable [`StrategySpec`] — the whole strategy document.
//!
//! Split out of `spec/mod.rs`; kept in `crate::spec::strategy` so paths like
//! `crate::spec::StrategySpec` still resolve via the `pub use` in `mod.rs`.

use std::sync::Arc;

use serde::Deserialize;

use fugazi::indicators::Position;
use fugazi::indicators::logic::Const;
use fugazi::prelude::*;
use fugazi::strategies::SingleAssetStrategy;

use super::signal::SignalSpec;
use super::source::SourceSpec;
use crate::dyn_indicator::{self, AsBool, AsReal, DynIndicator};

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
    fn exit(&self, anchor: &Position, schema: &Arc<Schema>) -> Box<dyn DynIndicator> {
        self.exit
            .as_ref()
            .map(|s| s.build(anchor, schema))
            .unwrap_or_else(|| {
                dyn_indicator::wrap(Const::<fugazi::types::Snapshot<String>>::new(false))
            })
    }
}

/// A whole `strategy.yml`: the traded symbol plus its long/short sides.
///
/// Sharing a subtree across sides is a plain YAML anchor: define `&name` at
/// the first use site and reference it with `*name` from every other site.
/// `serde_norway` resolves aliases before deserialization, so the typed spec
/// only ever sees the fully inlined tree.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StrategySpec {
    pub symbol: String,
    #[serde(default)]
    pub long: Option<SideSpec>,
    #[serde(default)]
    pub short: Option<SideSpec>,
    /// Optional **position-sizing multiplier** — a real-valued source read on
    /// every entry (or reversal) and multiplied into the value-fraction
    /// magnitude. Defaults to a constant `1.0` (all-in). Direction comes from
    /// the entry side; a negative reading is not meaningful, and a `None`
    /// reading skips the trade for that bar (safe default — build a well-defined
    /// fallback into the spec if that isn't what you want).
    #[serde(default)]
    pub sizing: Option<Box<SourceSpec>>,
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
    /// `schema` is the overlay [`Schema`] the atom stream carries — the
    /// `!get`-shaped leaves resolve their column names + types against it at
    /// build time. Pass [`Schema::empty()`] when there is no overlay side
    /// channel; `!get` will then panic with an "unknown key" that mentions
    /// the empty registered-keys list.
    ///
    /// No automatic wrapping — every signal / level is built exactly as the
    /// YAML describes it. If you want to gate an entry on stability, compose
    /// [`Unstable`](fugazi::indicators::Unstable) at the signal level to opt a
    /// subtree out of the strategy-readiness wait.
    pub fn build(&self, schema: &Arc<Schema>) -> DynSingleStrategy {
        let mut strat = SingleAssetStrategy::new(self.symbol.clone());
        // One position per strategy, shared by every `entry`/`peak`/`trough` leaf
        // in the sides' signals and stop levels.
        let anchor = strat.position();
        if let Some(long) = &self.long {
            strat = strat.long_on(
                AsBool::new(long.enter.build(&anchor, schema)),
                AsBool::new(long.exit(&anchor, schema)),
            );
            if let Some(sl) = &long.stop_loss {
                strat = strat.long_stop_loss(AsReal::new(sl.build(&anchor, schema)));
            }
            if let Some(tp) = &long.take_profit {
                strat = strat.long_take_profit(AsReal::new(tp.build(&anchor, schema)));
            }
        }
        if let Some(short) = &self.short {
            strat = strat.short_on(
                AsBool::new(short.enter.build(&anchor, schema)),
                AsBool::new(short.exit(&anchor, schema)),
            );
            if let Some(sl) = &short.stop_loss {
                strat = strat.short_stop_loss(AsReal::new(sl.build(&anchor, schema)));
            }
            if let Some(tp) = &short.take_profit {
                strat = strat.short_take_profit(AsReal::new(tp.build(&anchor, schema)));
            }
        }
        if let Some(sizing) = &self.sizing {
            strat = strat.position_sizing(AsReal::new(sizing.build(&anchor, schema)));
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
    type Input = fugazi::types::Snapshot<String>;
    type Symbol = String;

    fn update(&mut self, snap: fugazi::types::Snapshot<String>) {
        self.inner.update(snap);
    }
    fn on_fill(&mut self, order: &Order<String>) {
        self.inner.on_fill(order);
    }
    fn is_ready(&self) -> bool {
        self.inner.is_ready()
    }
    fn trade(&self, wallet: &mut dyn Wallet<String>) {
        self.inner.trade(wallet);
    }
    fn reset(&mut self) {
        self.inner.reset();
    }
}
