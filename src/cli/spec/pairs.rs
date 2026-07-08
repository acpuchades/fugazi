//! YAML-deserializable [`PairsStrategySpec`] — a two-symbol pair-trading spec.
//!
//! Mirrors [`super::StrategySpec`] for a two-leg strategy: two symbols (`left`,
//! `right`), one enter/exit signal pair, and optional spread stop-loss /
//! take-profit levels. Compared against the running `close_left − close_right`
//! spread the wallet-facing strategy computes internally.

use std::sync::Arc;

use serde::Deserialize;

use fugazi::indicators::Position;
use fugazi::indicators::logic::Const;
use fugazi::prelude::*;
use fugazi::strategies::PairsStrategy;

use super::signal::SignalSpec;
use super::source::SourceSpec;
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
/// As with [`super::StrategySpec`], a subtree is shared across sites via a
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
    pub stop_loss: Option<Box<SourceSpec>>,
    /// Optional spread take-profit level — the pair flattens when the running
    /// spread reads at or above this level.
    #[serde(default)]
    pub take_profit: Option<Box<SourceSpec>>,
}

impl PairsStrategySpec {
    /// Parse a YAML pairs-strategy document, resolving `param` placeholders
    /// against `params` first (see [`crate::params`]).
    ///
    /// Same two-pass shape as [`super::StrategySpec::from_text_with_params`]:
    /// the document is normalized to an untyped [`serde_json::Value`], every
    /// placeholder node is rewritten to its resolved value, and only then is
    /// the result deserialized into the typed spec.
    pub fn from_text_with_params(
        text: &str,
        params: &std::collections::HashMap<String, serde_json::Value>,
    ) -> anyhow::Result<Self> {
        let value = crate::input::parse_value(text)?;
        let value = crate::params::substitute(value, params)?;
        Ok(serde_json::from_value(value)?)
    }

    /// Build a spec's exit signal, defaulting a missing one to constant-`false`
    /// (matching the unwired slot in [`PairsStrategy::new`]).
    fn exit(&self, anchor: &Position, schema: &Arc<Schema>) -> Box<dyn DynIndicator> {
        self.exit
            .as_ref()
            .map(|s| s.build(anchor, schema))
            .unwrap_or_else(|| {
                crate::dyn_indicator::wrap(Const::<fugazi::types::Snapshot<String>>::new(false))
            })
    }

    /// Build the live [`DynPairsStrategy`] this spec describes.
    ///
    /// `schema` is the overlay [`Schema`] the atom stream carries — the
    /// `!get`-shaped leaves resolve their column names + types against it at
    /// build time. Level expressions that reference the strategy's `Position`
    /// (`entry` / `peak` / `trough`) anchor on the **left** leg — a rare choice
    /// since a spread-based level typically doesn't need the per-leg entry
    /// price, but present for symmetry with [`super::StrategySpec`].
    pub fn build(&self, schema: &Arc<Schema>) -> DynPairsStrategy {
        let strat = PairsStrategy::new(self.left.clone(), self.right.clone());
        // Anchor level expressions on the left leg's position (see doc note).
        let anchor = strat.left_position();
        let enter = AsBool::new(self.enter.build(&anchor, schema));
        let exit = AsBool::new(self.exit(&anchor, schema));
        let mut strat = strat.on(enter, exit);
        if let Some(sl) = &self.stop_loss {
            strat = strat.spread_stop_loss(AsReal::new(sl.build(&anchor, schema)));
        }
        if let Some(tp) = &self.take_profit {
            strat = strat.spread_take_profit(AsReal::new(tp.build(&anchor, schema)));
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
        let _built = spec.build(&Schema::empty());
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
        let _built = spec.build(&Schema::empty());
    }
}
