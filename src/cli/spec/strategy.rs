//! YAML-deserializable [`SingleStrategySpec`] — the whole strategy document.
//!
//! Split out of `spec/mod.rs`; kept in `crate::spec::strategy` so paths like
//! `crate::spec::SingleStrategySpec` still resolve via the `pub use` in `mod.rs`.

use std::sync::Arc;

use serde::Deserialize;

use fugazi::indicators::{Book, Position};
use fugazi::indicators::logic::Const;
use fugazi::prelude::*;
use fugazi::strategies::SingleAssetStrategy;

use super::signal::SignalSpec;
use super::expr::ExprSpec;
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
    pub stop_loss: Option<Box<ExprSpec>>,
    /// An optional take-profit price level (a source). The side flattens when the
    /// favourable extreme of the bar reaches it.
    #[serde(default)]
    pub take_profit: Option<Box<ExprSpec>>,
}

impl SideSpec {
    /// Build this side's exit signal, defaulting a missing one to constant-`false`
    /// (matching the unwired slots in [`SingleAssetStrategy::new`]).
    fn exit(
        &self,
        anchor: &Position,
        book: &Book,
        schema: &Arc<Schema>,
    ) -> Box<dyn DynIndicator> {
        self.exit
            .as_ref()
            .map(|s| s.build(anchor, book, schema))
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
pub struct SingleStrategySpec {
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
    pub sizing: Option<Box<ExprSpec>>,
}

impl SingleStrategySpec {
    /// Parse a YAML strategy document, splicing in every `!import`ed file and
    /// resolving `!param` placeholders against `params` first (see
    /// [`super::load_value`], [`crate::imports`], [`crate::params`]).
    ///
    /// Untyped-first: the document is normalized to a [`serde_json::Value`]
    /// (via [`crate::convert::yaml_to_json`], so `!tags` become serde_json's
    /// singleton-map external-tag form), every import and placeholder node is
    /// rewritten to its resolved value, and only then is the result deserialized
    /// into the typed spec — so a param can stand in for a number, a symbol, or
    /// any other concretely-typed field, and an import for any subtree.
    ///
    /// Import paths resolve against `base`, the importing document's own
    /// directory ([`crate::input::Source::base_dir`]).
    ///
    /// The CLI's top-level Single-strategy load goes through
    /// [`StrategyRef::from_text_with_params_in`](super::preset::StrategyRef::from_text_with_params_in)
    /// (which also accepts a preset tag) rather than this directly; kept as
    /// the typed single-spec loader the spec tests use.
    #[allow(dead_code)]
    pub fn from_text_with_params_in(
        text: &str,
        params: &std::collections::HashMap<String, serde_json::Value>,
        base: &std::path::Path,
    ) -> anyhow::Result<Self> {
        Ok(serde_json::from_value(super::load_value(
            text, params, base,
        )?)?)
    }

    /// [`from_text_with_params_in`](Self::from_text_with_params_in) with imports
    /// resolved against the working directory. A test convenience: every CLI
    /// call site has a [`Source`](crate::input::Source) and passes its
    /// `base_dir()` (which is already `.` for inline text).
    #[cfg(test)]
    pub fn from_text_with_params(
        text: &str,
        params: &std::collections::HashMap<String, serde_json::Value>,
    ) -> anyhow::Result<Self> {
        Self::from_text_with_params_in(text, params, std::path::Path::new("."))
    }

    /// Build the live [`DynSingleStrategy`] this spec describes.
    ///
    /// `initial_equity` seeds the strategy's [`Book`] anchor — it should
    /// match the wallet's starting cash for the book-anchored sizing
    /// recipes (`!drawdown_throttle`, `!equity_vol_target`,
    /// `!fractional_kelly`) to read meaningful numbers. The CLI threads
    /// `--cash` through to this parameter.
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
    pub fn build(&self, initial_equity: Real, schema: &Arc<Schema>) -> DynSingleStrategy {
        let mut strat =
            SingleAssetStrategy::with_initial_equity(self.symbol.clone(), initial_equity);
        // One position + book per strategy, shared by every `entry`/`peak`/`trough`
        // leaf (position) and every book-anchored sizing recipe (book).
        let anchor = strat.position();
        let book = strat.book();
        if let Some(long) = &self.long {
            strat = strat.long_on(
                AsBool::new(long.enter.build(&anchor, &book, schema)),
                AsBool::new(long.exit(&anchor, &book, schema)),
            );
            if let Some(sl) = &long.stop_loss {
                strat = strat.long_stop_loss(AsReal::new(sl.build(&anchor, &book, schema)));
            }
            if let Some(tp) = &long.take_profit {
                strat = strat.long_take_profit(AsReal::new(tp.build(&anchor, &book, schema)));
            }
        }
        if let Some(short) = &self.short {
            strat = strat.short_on(
                AsBool::new(short.enter.build(&anchor, &book, schema)),
                AsBool::new(short.exit(&anchor, &book, schema)),
            );
            if let Some(sl) = &short.stop_loss {
                strat = strat.short_stop_loss(AsReal::new(sl.build(&anchor, &book, schema)));
            }
            if let Some(tp) = &short.take_profit {
                strat = strat.short_take_profit(AsReal::new(tp.build(&anchor, &book, schema)));
            }
        }
        if let Some(sizing) = &self.sizing {
            strat = strat.position_sizing(AsReal::new(sizing.build(&anchor, &book, schema)));
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

impl DynSingleStrategy {
    /// Wrap an already-built [`SingleAssetStrategy<String>`] — the seam the
    /// [`StrategyPreset`](super::preset::StrategyPreset) catalogue tags use to
    /// hand a ready-made strategy (built by the `fugazi::strategies` free
    /// functions) into the same `DynSingleStrategy` the YAML `SingleStrategySpec`
    /// path produces.
    pub(crate) fn from_single(inner: SingleAssetStrategy<String>) -> Self {
        Self { inner }
    }
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
