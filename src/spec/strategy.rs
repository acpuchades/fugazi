//! YAML-deserializable [`SingleStrategySpec`] — the whole strategy document.
//!
//! Split out of `spec/mod.rs`; kept in `crate::spec::strategy` so paths like
//! `crate::spec::SingleStrategySpec` still resolve via the `pub use` in `mod.rs`.

use std::sync::Arc;

use serde::Deserialize;

use crate::indicators::logic::ValueBool;
use crate::indicators::{Book, Position};
use crate::prelude::*;
use crate::strategies::SingleAssetStrategy;

use super::expr::{BoolNode, RealNode, Root};
use super::meta::Meta;
use super::root::{RootKey, RootSpec};
use crate::runtime::{AnyChain, any};
use crate::types::Symbol;

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
    pub enter: BoolNode,
    #[serde(default)]
    pub exit: Option<BoolNode>,
    /// An optional stop-loss price level (a source). The side flattens when the
    /// adverse extreme of the bar reaches it. A `peak` / `trough` source makes it
    /// a trailing stop.
    #[serde(default)]
    pub stop_loss: Option<Box<RealNode>>,
    /// An optional take-profit price level (a source). The side flattens when the
    /// favourable extreme of the bar reaches it.
    #[serde(default)]
    pub take_profit: Option<Box<RealNode>>,
}

impl SideSpec {
    /// Build this side's exit signal, defaulting a missing one to constant-`false`
    /// (matching the unwired slots in [`SingleAssetStrategy::new`]).
    fn exit(
        &self,
        anchor: &Position,
        book: &Book,
        schema: &Arc<Schema>,
        root: Root<'_>,
    ) -> Result<AnyChain, String> {
        match &self.exit {
            Some(s) => s.try_build(anchor, book, None, schema, root),
            None => Ok(any(ValueBool::<crate::types::Snapshot<Symbol>>::new(false))),
        }
    }
}

/// A whole `strategy.yml`: the evaluation root plus its long/short sides.
///
/// Sharing a subtree across sides is a plain YAML anchor: define `&name` at
/// the first use site and reference it with `*name` from every other site.
/// `serde_norway` resolves aliases before deserialization, so the typed spec
/// only ever sees the fully inlined tree.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SingleStrategySpec {
    /// The document's **evaluation root**: the series every `source:`-omitted
    /// leaf reads, and the instrument this strategy trades.
    ///
    /// An ordinary atom-valued expression, so `!param` reaches it like any
    /// other slot — which is what lets one document be swept over instruments.
    /// `root: BTCUSDT` is sugar for `root: !pick { symbol: BTCUSDT }`. The
    /// traded symbol is recovered from it by
    /// [`RootSpec::sole_symbol`](crate::spec::RootSpec::sole_symbol); a root
    /// naming none or several is a build error, not a parse error.
    ///
    /// **Optional.** Omitted, it defaults to `!pick { symbol: !param { key:
    /// SYMBOL }, freq: !param { key: FREQ } }` — spliced in by
    /// [`root::apply_default`](crate::spec::root::apply_default) before
    /// substitution, with each placeholder dropping its key when unset, so a
    /// document supplying neither lands on the sole-atom root. A tree loaded
    /// through a path that does *not* splice it (a portfolio child, a
    /// `serde_json::from_value` on a hand-built map) defaults the same way, via
    /// [`RootSpec::sole`](crate::spec::RootSpec::sole).
    #[serde(default)]
    pub root: RootSpec,
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
    pub sizing: Option<Box<RealNode>>,

    /// Optional **rebalance gate** — a boolean signal deciding, on each
    /// bar, whether the open position is resized to the current sizing
    /// target. Defaults to `!never` — sizing only reads on transitions,
    /// matching pre-refactor behavior. Useful for a vol-targeted or
    /// Kelly-scaled single-asset strategy that wants to adjust an open
    /// position when the target drifts.
    #[serde(default)]
    pub rebalance_on: Option<BoolNode>,

    /// Free-form document metadata for external tooling. Parsed, carried, and
    /// never interpreted — see [`spec::meta`](crate::spec::meta).
    #[serde(default)]
    pub meta: Option<Meta>,
}

impl SingleStrategySpec {
    /// Parse a YAML strategy document, splicing in every `!import`ed file and
    /// resolving `!param` placeholders against `params` first (see
    /// [`super::load_value`], [`crate::spec::imports`], [`crate::spec::params`]).
    ///
    /// Untyped-first: the document is normalized to a [`serde_json::Value`]
    /// (via [`crate::spec::convert::yaml_to_json`], so `!tags` become serde_json's
    /// singleton-map external-tag form), every import and placeholder node is
    /// rewritten to its resolved value, and only then is the result deserialized
    /// into the typed spec — so a param can stand in for a number, a symbol, or
    /// any other concretely-typed field, and an import for any subtree.
    ///
    /// Import paths resolve against `base`, the importing document's own
    /// directory ([`crate::spec::input::Source::base_dir`]); `root` is the
    /// confinement boundary (see [`super::load_value`]) — pass `base` for both
    /// for the historical "confined to its own directory" default.
    ///
    /// The CLI's top-level Single-strategy load goes through
    /// [`StrategyRef::from_text_with_params_in`](super::preset::StrategyRef::from_text_with_params_in)
    /// (which also accepts a preset tag) rather than this directly; kept as
    /// the typed single-spec loader the spec tests use.
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
        serde_json::from_value(value)
            .with_context(|| format!("building single-asset strategy from {label}"))
    }

    /// [`from_text_with_params_in`](Self::from_text_with_params_in) with imports
    /// resolved against the working directory and an `(inline)` source label.
    /// A test convenience: every CLI call site has a
    /// [`Source`](crate::spec::input::Source) and passes its `base_dir()` (already `.`
    /// for inline text) and its `label()`.
    #[cfg(test)]
    pub fn from_text_with_params(
        text: &str,
        params: &std::collections::HashMap<String, serde_json::Value>,
    ) -> anyhow::Result<Self> {
        Self::from_text_with_params_in(
            text,
            params,
            std::path::Path::new("."),
            std::path::Path::new("."),
            "(inline)",
        )
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
    /// [`Unstable`](crate::indicators::Unstable) at the signal level to opt a
    /// subtree out of the strategy-readiness wait.
    pub fn build(&self, initial_equity: Real, schema: &Arc<Schema>) -> DynSingleStrategy {
        self.try_build(initial_equity, schema)
            .unwrap_or_else(|e| panic!("{e}"))
    }

    /// The fallible twin of [`build`](Self::build): every slot is built through
    /// [`NodeSpec::try_build`](crate::spec::expr::NodeSpec::try_build) / [`NodeSpec::try_build`](crate::spec::expr::NodeSpec::try_build), so a bad expression
    /// anywhere in the document comes back as a message with its tag trail
    /// instead of aborting the process.
    pub fn try_build(
        &self,
        initial_equity: Real,
        schema: &Arc<Schema>,
    ) -> Result<DynSingleStrategy, String> {
        crate::spec::runnable::check_seed(initial_equity)?;
        let mut strat = SingleAssetStrategy::with_initial_equity(
            crate::types::symbol(&self.root.sole_symbol(RootKey::ROOT, "single-asset")?),
            initial_equity,
        );
        // One position + book per strategy, shared by every `entry`/`peak`/`trough`
        // leaf (position) and every book-anchored sizing recipe (book).
        let anchor = strat.position();
        let book = strat.book();
        // The blessed series: every `source:`-omitted leaf in this spec reads
        // the series the strategy trades. Declaring `root:` and having a
        // bare `!close` mean something else would be indefensible — and it
        // lets a single-asset spec run against a multi-symbol `--series`
        // frame instead of tripping the sole-atom panic.
        let root = Root::blessed(&self.root);
        if let Some(long) = &self.long {
            strat = strat.long_on(
                (long.enter.try_build(&anchor, &book, None, schema, root)?).into_bool()?,
                (long.exit(&anchor, &book, schema, root)?).into_bool()?,
            );
            if let Some(sl) = &long.stop_loss {
                strat = strat.long_stop_loss(
                    (sl.try_build(&anchor, &book, None, schema, root)?).into_real()?,
                );
            }
            if let Some(tp) = &long.take_profit {
                strat = strat.long_take_profit(
                    (tp.try_build(&anchor, &book, None, schema, root)?).into_real()?,
                );
            }
        }
        if let Some(short) = &self.short {
            strat = strat.short_on(
                (short.enter.try_build(&anchor, &book, None, schema, root)?).into_bool()?,
                (short.exit(&anchor, &book, schema, root)?).into_bool()?,
            );
            if let Some(sl) = &short.stop_loss {
                strat = strat.short_stop_loss(
                    (sl.try_build(&anchor, &book, None, schema, root)?).into_real()?,
                );
            }
            if let Some(tp) = &short.take_profit {
                strat = strat.short_take_profit(
                    (tp.try_build(&anchor, &book, None, schema, root)?).into_real()?,
                );
            }
        }
        if let Some(sizing) = &self.sizing {
            strat = strat.position_sizing(
                (sizing.try_build(&anchor, &book, None, schema, root)?).into_real()?,
            );
        }
        if let Some(rebalance) = &self.rebalance_on {
            strat = strat.rebalance_on(
                (rebalance.try_build(&anchor, &book, None, schema, root)?).into_bool()?,
            );
        }
        Ok(DynSingleStrategy { inner: strat })
    }
}

// ---------------------------------------------------------------------------
// DynSingleStrategy: CLI-owned wrapper around SingleAssetStrategy<Symbol>
// ---------------------------------------------------------------------------

/// The CLI's built-strategy handle. Wraps a [`SingleAssetStrategy<Symbol>`]
/// whose entry/exit signals and protective levels came from runtime-typed
/// [`AnyChain`]s (coerced into typed [`Signal`] /
/// real levels by `into_bool` / `into_real` at construction).
///
/// Implements [`Strategy`] by delegation, so it drops into
/// [`crate::backtest::run`] unchanged.
pub struct DynSingleStrategy {
    inner: SingleAssetStrategy<Symbol>,
}

impl DynSingleStrategy {
    /// Wrap an already-built [`SingleAssetStrategy<Symbol>`] — the seam the
    /// [`StrategyPreset`](super::preset::StrategyPreset) catalogue tags use to
    /// hand a ready-made strategy (built by the `crate::strategies` free
    /// functions) into the same `DynSingleStrategy` the YAML `SingleStrategySpec`
    /// path produces.
    pub(crate) fn from_single(inner: SingleAssetStrategy<Symbol>) -> Self {
        Self { inner }
    }

    /// Grid-wide readiness across the wired signals, protective levels, and
    /// sizing indicator — pass-through to
    /// [`SingleAssetStrategy::stable_bars`].
    pub fn stable_bars(&self) -> usize {
        self.inner.stable_bars()
    }

    /// Warm-up-only readiness (ignoring IIR settling) — pass-through to
    /// [`SingleAssetStrategy::warm_up_bars`]. Used by
    /// `optimize --walkforward --keep-unstable` to compute the prefix skip
    /// under the opt-out.
    pub fn warm_up_bars(&self) -> usize {
        self.inner.warm_up_bars()
    }

    /// The strategy's shared [`Book<Symbol>`] — hand-off point for
    /// portfolio-level weight-share templates that want to read
    /// `!drawdown` / `!return_per_bar` / `!trade_return` against this
    /// child's book.
    pub fn book(&self) -> Book<Symbol> {
        self.inner.book()
    }

    /// Serialize the wrapped strategy's runtime state for run resuming.
    pub fn save_state(&self) -> serde_json::Value {
        self.inner.save_state()
    }

    /// Restore state produced by [`save_state`](Self::save_state).
    pub fn restore_state(&mut self, state: &serde_json::Value) -> Result<(), String> {
        self.inner.restore_state(state)
    }
}

impl Strategy for DynSingleStrategy {
    type Input = crate::types::Snapshot<Symbol>;
    type Symbol = Symbol;

    fn update(&mut self, snap: crate::types::Snapshot<Symbol>) {
        self.inner.update(snap);
    }
    fn on_fill(&mut self, order: &Order<Symbol>) {
        self.inner.on_fill(order);
    }
    fn is_ready(&self) -> bool {
        self.inner.is_ready()
    }
    fn trade(&self, wallet: &mut dyn Wallet<Symbol>) {
        self.inner.trade(wallet);
    }
    fn force_rebalance(&mut self, hold: Option<&[Symbol]>) {
        self.inner.force_rebalance(hold);
    }
    fn reset(&mut self) {
        self.inner.reset();
    }
    fn save_state(&self) -> serde_json::Value {
        self.inner.save_state()
    }
    fn load_state(&mut self, state: &serde_json::Value) -> Result<(), String> {
        self.inner.restore_state(state)
    }
}
