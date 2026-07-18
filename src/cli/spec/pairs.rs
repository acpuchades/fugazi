//! YAML-deserializable [`PairsStrategySpec`] — a two-symbol pair-trading spec.
//!
//! Mirrors [`super::SingleStrategySpec`] for a two-leg strategy: two symbols (`left`,
//! `right`), one enter/exit signal pair, and optional spread stop-loss /
//! take-profit levels. Compared against the running `close_left − close_right`
//! spread the wallet-facing strategy computes internally.

use std::sync::Arc;

use serde::Deserialize;

use fugazi::indicators::{Book, Position};
use fugazi::indicators::logic::Const;
use fugazi::prelude::*;
use fugazi::strategies::PairsStrategy;

use super::signal::SignalSpec;
use super::expr::ExprSpec;
use crate::dyn_indicator::{AsBool, AsReal, DynIndicator};

/// A whole `pairs.yml`: the two traded symbols plus one enter/exit signal pair
/// and optional spread levels.
///
/// Inside signal / level expressions, atom-input leaves (`!close`, `!high`, …)
/// **must** be rooted through `!pick { symbol: <sym> }` — a bare `!close` uses
/// the empty-selector `Pick` which panics on multi-asset snapshots. The typical
/// shape:
///
/// ```yaml
/// left: BTC
/// right: ETH
/// enter: !crosses_below
///   lhs: !sub
///     lhs: !close { source: !pick { symbol: BTC } }
///     rhs: !close { source: !pick { symbol: ETH } }
///   rhs: !sma
///     period: 20
///     source: !sub
///       lhs: !close { source: !pick { symbol: BTC } }
///       rhs: !close { source: !pick { symbol: ETH } }
/// exit: !crosses_above { … }
/// stop_loss:   !value -50.0    # spread level (close_L - close_R)
/// take_profit: !value  50.0
/// ```
///
/// As with [`super::SingleStrategySpec`], a subtree is shared across sites via a
/// plain YAML anchor (`&name` / `*name`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairsStrategySpec {
    pub left: String,
    pub right: String,
    pub enter: SignalSpec,
    #[serde(default)]
    pub exit: Option<SignalSpec>,
    /// Optional spread stop-loss level — the pair flattens when the running
    /// spread reads at or below this level.
    #[serde(default)]
    pub stop_loss: Option<Box<ExprSpec>>,
    /// Optional spread take-profit level — the pair flattens when the running
    /// spread reads at or above this level.
    #[serde(default)]
    pub take_profit: Option<Box<ExprSpec>>,
    /// Optional **position-sizing multiplier** — a real-valued source scaling
    /// the pair's gross exposure. Each leg entries at `value_frac(0.5 * m)`.
    /// Defaults to a constant `1.0` (1.0 gross, dollar-neutral); a `None`
    /// reading skips the trade for that bar.
    #[serde(default)]
    pub sizing: Option<Box<ExprSpec>>,

    /// Optional **rebalance gate** — a boolean signal deciding, on each
    /// bar, whether both legs are resized to the current sizing target.
    /// Defaults to `!never` — sizing only reads on entry, matching
    /// pre-refactor behavior.
    #[serde(default)]
    pub rebalance_on: Option<SignalSpec>,
}

impl PairsStrategySpec {
    /// Parse a YAML pairs-strategy document, resolving `param` placeholders
    /// against `params` first (see [`crate::params`]).
    ///
    /// Same two-pass shape as [`super::SingleStrategySpec::from_text_with_params`]:
    /// the document is normalized to an untyped [`serde_json::Value`], every
    /// placeholder node is rewritten to its resolved value, and only then is
    /// the result deserialized into the typed spec.
    pub fn from_text_with_params_in(
        text: &str,
        params: &std::collections::HashMap<String, serde_json::Value>,
        base: &std::path::Path,
        label: &str,
    ) -> anyhow::Result<Self> {
        use anyhow::Context;
        let value = super::load_value(text, params, base, label)?;
        serde_json::from_value(value)
            .with_context(|| format!("building pairs strategy from {label}"))
    }

    /// [`from_text_with_params_in`](Self::from_text_with_params_in) with imports
    /// resolved against the working directory and an `(inline)` source label —
    /// a test convenience (the CLI passes the strategy source's `base_dir()`
    /// and `label()`).
    #[cfg(test)]
    pub fn from_text_with_params(
        text: &str,
        params: &std::collections::HashMap<String, serde_json::Value>,
    ) -> anyhow::Result<Self> {
        Self::from_text_with_params_in(text, params, std::path::Path::new("."), "(inline)")
    }

    /// Build a spec's exit signal, defaulting a missing one to constant-`false`
    /// (matching the unwired slot in [`PairsStrategy::new`]).
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
                crate::dyn_indicator::wrap(Const::<fugazi::types::Snapshot<String>>::new(false))
            })
    }

    /// Build the live [`DynPairsStrategy`] this spec describes.
    ///
    /// `initial_equity` seeds the pair's [`Book`] anchor — match the
    /// wallet's starting cash for the book-anchored sizing recipes
    /// (`!drawdown_throttle`, `!equity_vol_target`, `!fractional_kelly`)
    /// to read meaningful numbers. The CLI threads `--cash` through to
    /// this parameter.
    ///
    /// `schema` is the overlay [`Schema`] the atom stream carries — the
    /// `!get`-shaped leaves resolve their column names + types against it at
    /// build time. Level expressions that reference the strategy's `Position`
    /// (`entry` / `peak` / `trough`) anchor on the **left** leg — a rare choice
    /// since a spread-based level typically doesn't need the per-leg entry
    /// price, but present for symmetry with [`super::SingleStrategySpec`].
    pub fn build(&self, initial_equity: Real, schema: &Arc<Schema>) -> DynPairsStrategy {
        let strat = PairsStrategy::with_initial_equity(
            self.left.clone(),
            self.right.clone(),
            initial_equity,
        );
        // Anchor level expressions on the left leg's position (see doc note).
        let anchor = strat.left_position();
        // Real Book shared with the strategy — book-anchored sizing tags
        // (`!drawdown_throttle`, `!equity_vol_target`, `!fractional_kelly`)
        // read the pair's aggregate equity curve.
        let book = strat.book();
        let enter = AsBool::new(self.enter.build(&anchor, &book, schema));
        let exit = AsBool::new(self.exit(&anchor, &book, schema));
        let mut strat = strat.on(enter, exit);
        if let Some(sl) = &self.stop_loss {
            strat = strat.spread_stop_loss(AsReal::new(sl.build(&anchor, &book, schema)));
        }
        if let Some(tp) = &self.take_profit {
            strat = strat.spread_take_profit(AsReal::new(tp.build(&anchor, &book, schema)));
        }
        if let Some(sizing) = &self.sizing {
            strat = strat.position_sizing(AsReal::new(sizing.build(&anchor, &book, schema)));
        }
        if let Some(rebalance) = &self.rebalance_on {
            strat = strat.rebalance_on(AsBool::new(rebalance.build(&anchor, &book, schema)));
        }
        DynPairsStrategy { inner: strat }
    }
}

/// The CLI's built pairs-strategy handle. Wraps a
/// [`PairsStrategy<String>`](fugazi::strategies::PairsStrategy) whose signals
/// and levels came from runtime-typed [`DynIndicator`]s.
///
/// Implements [`Strategy`](fugazi::Strategy) by delegation, so it drops into
/// [`fugazi::backtest::run`] unchanged.
pub struct DynPairsStrategy {
    inner: PairsStrategy<String>,
}

impl DynPairsStrategy {
    /// Grid-wide readiness — pass-through to
    /// [`PairsStrategy::stable_period`]. All chains are held eagerly, so
    /// this reads directly (no lazy-probe needed like basket / multi).
    pub fn stable_period(&self) -> usize {
        self.inner.stable_period()
    }

    /// Warm-up-only readiness (ignoring IIR settling) — pass-through to
    /// [`PairsStrategy::warm_up_period`]. Used by
    /// `optimize --walkforward --keep-unstable`.
    pub fn warm_up_period(&self) -> usize {
        self.inner.warm_up_period()
    }

    /// The strategy's shared [`Book<String>`] — hand-off point for
    /// portfolio-level weight-share templates that want to read
    /// `!drawdown` / `!return_per_bar` / `!trade_return` against this
    /// child's aggregate two-leg book.
    pub fn book(&self) -> Book<String> {
        self.inner.book()
    }
}

impl Strategy for DynPairsStrategy {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_pairs_spec_with_signals_and_levels() {
        let yaml = r#"
            left: BTC
            right: ETH
            enter: !below
              source: !sub
                lhs: !close { source: !pick { symbol: BTC } }
                rhs: !close { source: !pick { symbol: ETH } }
              level: -1.0
            exit: !above
              source: !sub
                lhs: !close { source: !pick { symbol: BTC } }
                rhs: !close { source: !pick { symbol: ETH } }
              level: 0.0
            stop_loss: !value -50.0
            take_profit: !value 50.0
        "#;
        let spec = PairsStrategySpec::from_text_with_params(
            yaml,
            &std::collections::HashMap::new(),
        )
        .unwrap();
        assert_eq!(spec.left, "BTC");
        assert_eq!(spec.right, "ETH");
        assert!(spec.stop_loss.is_some());
        assert!(spec.take_profit.is_some());
        let _built = spec.build(1_000.0, &Schema::empty());
    }

    #[test]
    fn parses_minimal_pairs_spec_with_only_enter() {
        let yaml = r#"
            left: BTC
            right: ETH
            enter: !value true
        "#;
        let spec = PairsStrategySpec::from_text_with_params(
            yaml,
            &std::collections::HashMap::new(),
        )
        .unwrap();
        assert!(spec.exit.is_none() && spec.stop_loss.is_none() && spec.take_profit.is_none());
        let _built = spec.build(1_000.0, &Schema::empty());
    }

    #[test]
    fn parses_pairs_spec_with_book_anchored_sizing() {
        // Verify that a book-anchored recipe (drawdown_throttle) parses and
        // builds against a real pair Book (not the dummy that used to be there).
        let yaml = r#"
            left: BTC
            right: ETH
            enter: !value true
            sizing: !drawdown_throttle { max_drawdown: 0.20 }
        "#;
        let spec = PairsStrategySpec::from_text_with_params(
            yaml,
            &std::collections::HashMap::new(),
        )
        .unwrap();
        assert!(spec.sizing.is_some());
        let _built = spec.build(10_000.0, &Schema::empty());
    }
}
