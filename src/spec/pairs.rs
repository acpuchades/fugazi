//! YAML-deserializable [`PairsStrategySpec`] — a two-symbol pair-trading spec.
//!
//! Mirrors [`super::SingleStrategySpec`] for a two-leg strategy: two symbols (`left`,
//! `right`), one enter/exit signal pair, and optional spread stop-loss /
//! take-profit levels. Compared against the running `close_left − close_right`
//! spread the wallet-facing strategy computes internally.

use std::sync::Arc;

use serde::Deserialize;

use crate::indicators::logic::ValueBool;
use crate::indicators::{Book, Position};
use crate::prelude::*;
use crate::strategies::PairsStrategy;

use super::expr::{BoolNode, RealNode, Root};
use super::meta::Meta;
use super::strategy::SideSpec;
use crate::runtime::AnyChain;
use crate::types::Symbol;

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
/// ## Trading both directions
///
/// The keys above describe the **long-spread** side (long `left`, short
/// `right`, profiting as the spread rises). A mean-reverting spread visits both
/// tails, so the other half is reached by adding a `short_spread:` block —
/// short `left`, long `right`, profiting as the spread falls:
///
/// ```yaml
/// left: BTC
/// right: ETH
///
/// long_spread:                             # spread cheap -> expect it to rise
///   enter: !below { source: *z, level: -2.0 }
///   exit:  !above { source: *z, level:  0.0 }
///   stop_loss: !sub { lhs: *ma, rhs: !mul { lhs: *sd, rhs: !value 4.0 } }
///
/// short_spread:                            # spread rich -> expect it to fall
///   enter: !above { source: *z, level:  2.0 }
///   exit:  !below { source: *z, level:  0.0 }
///   stop_loss: !add { lhs: *ma, rhs: !mul { lhs: *sd, rhs: !value 4.0 } }
/// ```
///
/// The two directions are inverse positions, so they are mutually exclusive in
/// time and share one capital pool at full notional. The short side's levels
/// are compared with **mirrored sense** — its stop fires when the spread rises
/// *above* the level, since that is its adverse direction.
///
/// The flat top-level `enter` / `exit` / `stop_loss` / `take_profit` keys remain
/// valid as a spelling of the long-spread side, so existing documents are
/// unaffected. Setting both them and a `long_spread:` block is an error.
///
/// As with [`super::SingleStrategySpec`], a subtree is shared across sites via a
/// plain YAML anchor (`&name` / `*name`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, try_from = "PairsStrategySpecRaw")]
pub struct PairsStrategySpec {
    pub left: String,
    pub right: String,
    /// The long-spread entry, in the flat spelling. Mutually exclusive with
    /// [`long_spread`](Self::long_spread); one of the two (or a
    /// [`short_spread`](Self::short_spread) block) must be present.
    #[serde(default)]
    pub enter: Option<BoolNode>,
    #[serde(default)]
    pub exit: Option<BoolNode>,
    /// Optional spread stop-loss level — the long-spread side flattens when the
    /// running spread reads at or below this level.
    #[serde(default)]
    pub stop_loss: Option<Box<RealNode>>,
    /// Optional spread take-profit level — the long-spread side flattens when
    /// the running spread reads at or above this level.
    #[serde(default)]
    pub take_profit: Option<Box<RealNode>>,
    /// The **long-spread** side (long `left` / short `right`) as a block — the
    /// symmetric spelling of the four flat keys above.
    #[serde(default)]
    pub long_spread: Option<SideSpec>,
    /// The **short-spread** side (short `left` / long `right`). Present only
    /// when the pair should also trade reversion from the rich tail; its
    /// `stop_loss` / `take_profit` compare with mirrored sense.
    #[serde(default)]
    pub short_spread: Option<SideSpec>,
    /// Optional **position-sizing multiplier** — a real-valued source scaling
    /// the pair's gross exposure. Each leg entries at `value_frac(0.5 * m)`.
    /// Defaults to a constant `1.0` (1.0 gross, dollar-neutral); a `None`
    /// reading skips the trade for that bar.
    #[serde(default)]
    pub sizing: Option<Box<RealNode>>,

    /// Optional **rebalance gate** — a boolean signal deciding, on each
    /// bar, whether both legs are resized to the current sizing target.
    /// Defaults to `!never` — sizing only reads on entry, matching
    /// pre-refactor behavior.
    #[serde(default)]
    pub rebalance_on: Option<BoolNode>,

    /// Free-form document metadata for external tooling. Parsed, carried, and
    /// never interpreted — see [`spec::meta`](crate::spec::meta).
    #[serde(default)]
    pub meta: Option<Meta>,
}

/// Deserialization mirror of [`PairsStrategySpec`], carrying the same fields
/// with a derived `Deserialize`.
///
/// The side-wiring rules are checked in the [`TryFrom`] below rather than in
/// [`PairsStrategySpec::build`], so `fugazi check strategy` — which validates a
/// document's *shape* without ever building it — catches them too, and so a
/// spec mistake surfaces as an error rather than a panic. Same reason
/// `NodeSpec` and `NodeSpec` deserialize through a raw mirror.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PairsStrategySpecRaw {
    left: String,
    right: String,
    #[serde(default)]
    enter: Option<BoolNode>,
    #[serde(default)]
    exit: Option<BoolNode>,
    #[serde(default)]
    stop_loss: Option<Box<RealNode>>,
    #[serde(default)]
    take_profit: Option<Box<RealNode>>,
    #[serde(default)]
    long_spread: Option<SideSpec>,
    #[serde(default)]
    short_spread: Option<SideSpec>,
    #[serde(default)]
    sizing: Option<Box<RealNode>>,
    #[serde(default)]
    rebalance_on: Option<BoolNode>,
    #[serde(default)]
    meta: Option<Meta>,
}

impl TryFrom<PairsStrategySpecRaw> for PairsStrategySpec {
    type Error = String;

    fn try_from(raw: PairsStrategySpecRaw) -> Result<Self, Self::Error> {
        let flat_present = raw.enter.is_some()
            || raw.exit.is_some()
            || raw.stop_loss.is_some()
            || raw.take_profit.is_some();
        if flat_present && raw.long_spread.is_some() {
            return Err(
                "a pairs document sets both the flat `enter`/`exit`/`stop_loss`/`take_profit` \
                 keys and a `long_spread:` block — they are two spellings of the same side \
                 (long left / short right); keep one"
                    .to_string(),
            );
        }
        if !flat_present && raw.long_spread.is_none() && raw.short_spread.is_none() {
            return Err(
                "a pairs document wires neither direction — give `long_spread:` (or the flat \
                 `enter:`) to trade the spread from the cheap tail, and/or `short_spread:` to \
                 trade it from the rich one"
                    .to_string(),
            );
        }
        Ok(PairsStrategySpec {
            left: raw.left,
            right: raw.right,
            enter: raw.enter,
            exit: raw.exit,
            stop_loss: raw.stop_loss,
            take_profit: raw.take_profit,
            long_spread: raw.long_spread,
            short_spread: raw.short_spread,
            sizing: raw.sizing,
            rebalance_on: raw.rebalance_on,
            meta: raw.meta,
        })
    }
}

impl PairsStrategySpec {
    /// Parse a YAML pairs-strategy document, resolving `param` placeholders
    /// against `params` first (see [`crate::spec::params`]).
    ///
    /// Same two-pass shape as `SingleStrategySpec::from_text_with_params`:
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

    /// The long-spread side, however it was spelled: either the `long_spread:`
    /// block or the four flat top-level keys.
    ///
    /// `None` when only `short_spread:` was given. The "both spellings" and
    /// "neither direction" cases are rejected at deserialization (see
    /// [`PairsStrategySpecRaw`]), so they cannot reach here.
    fn long_side(&self) -> Option<std::borrow::Cow<'_, SideSpec>> {
        if let Some(block) = &self.long_spread {
            return Some(std::borrow::Cow::Borrowed(block));
        }
        let enter = self.enter.clone()?;
        Some(std::borrow::Cow::Owned(SideSpec {
            enter,
            exit: self.exit.clone(),
            stop_loss: self.stop_loss.clone(),
            take_profit: self.take_profit.clone(),
        }))
    }

    /// Build a side's exit signal, defaulting a missing one to constant-`false`
    /// (matching the unwired slots in [`PairsStrategy::new`]).
    fn exit_of(
        side: Option<&SideSpec>,
        anchor: &Position,
        book: &Book,
        schema: &Arc<Schema>,
    ) -> Result<AnyChain, String> {
        match side.and_then(|s| s.exit.as_ref()) {
            Some(s) => s.try_build(anchor, book, None, schema, Root::ambiguous("pairs")),
            None => Ok(crate::runtime::any(ValueBool::<
                crate::types::Snapshot<Symbol>,
            >::new(false))),
        }
    }

    /// Build a side's entry signal, defaulting an absent side to
    /// constant-`false` so that direction never opens.
    fn enter_of(
        side: Option<&SideSpec>,
        anchor: &Position,
        book: &Book,
        schema: &Arc<Schema>,
    ) -> Result<AnyChain, String> {
        match side {
            Some(s) => s
                .enter
                .try_build(anchor, book, None, schema, Root::ambiguous("pairs")),
            None => Ok(crate::runtime::any(ValueBool::<
                crate::types::Snapshot<Symbol>,
            >::new(false))),
        }
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
        self.try_build(initial_equity, schema)
            .unwrap_or_else(|e| panic!("{e}"))
    }

    /// The fallible twin of [`build`](Self::build) — see
    /// [`SingleStrategySpec::try_build`](crate::spec::SingleStrategySpec::try_build).
    pub fn try_build(
        &self,
        initial_equity: Real,
        schema: &Arc<Schema>,
    ) -> Result<DynPairsStrategy, String> {
        // The document holds the leg names as `String`; interning here means
        // every later clone of them (per bar, per fill) is a refcount bump.
        let strat = PairsStrategy::with_initial_equity(
            crate::types::symbol(&self.left),
            crate::types::symbol(&self.right),
            initial_equity,
        );
        // Anchor level expressions on the left leg's position (see doc note).
        let anchor = strat.left_position();
        // Every `build` below passes `root: None` — a pair has no *blessed*
        // series. `left` and `right` are peers, so "this series" is undefined
        // and a bare `!close` would have to guess; every leaf names its asset
        // with `!pick { symbol: ... }` and the sole-atom panic stays as the
        // guard against a spec that forgot to.
        // Real Book shared with the strategy — book-anchored sizing tags
        // (`!drawdown_throttle`, `!equity_vol_target`, `!fractional_kelly`)
        // read the pair's aggregate equity curve.
        let book = strat.book();
        let long = self.long_side();
        let long = long.as_deref();
        let short = self.short_spread.as_ref();

        let mut strat = strat.long_spread_on(
            (Self::enter_of(long, &anchor, &book, schema)?).into_bool()?,
            (Self::exit_of(long, &anchor, &book, schema)?).into_bool()?,
        );
        if short.is_some() {
            strat = strat.short_spread_on(
                (Self::enter_of(short, &anchor, &book, schema)?).into_bool()?,
                (Self::exit_of(short, &anchor, &book, schema)?).into_bool()?,
            );
        }
        if let Some(sl) = long.and_then(|s| s.stop_loss.as_ref()) {
            strat = strat.long_spread_stop_loss(
                (sl.try_build(&anchor, &book, None, schema, Root::ambiguous("pairs"))?)
                    .into_real()?,
            );
        }
        if let Some(tp) = long.and_then(|s| s.take_profit.as_ref()) {
            strat = strat.long_spread_take_profit(
                (tp.try_build(&anchor, &book, None, schema, Root::ambiguous("pairs"))?)
                    .into_real()?,
            );
        }
        if let Some(sl) = short.and_then(|s| s.stop_loss.as_ref()) {
            strat = strat.short_spread_stop_loss(
                (sl.try_build(&anchor, &book, None, schema, Root::ambiguous("pairs"))?)
                    .into_real()?,
            );
        }
        if let Some(tp) = short.and_then(|s| s.take_profit.as_ref()) {
            strat = strat.short_spread_take_profit(
                (tp.try_build(&anchor, &book, None, schema, Root::ambiguous("pairs"))?)
                    .into_real()?,
            );
        }
        if let Some(sizing) = &self.sizing {
            strat = strat.position_sizing(
                (sizing.try_build(&anchor, &book, None, schema, Root::ambiguous("pairs"))?)
                    .into_real()?,
            );
        }
        if let Some(rebalance) = &self.rebalance_on {
            strat = strat.rebalance_on(
                (rebalance.try_build(&anchor, &book, None, schema, Root::ambiguous("pairs"))?)
                    .into_bool()?,
            );
        }
        Ok(DynPairsStrategy { inner: strat })
    }
}

/// The CLI's built pairs-strategy handle. Wraps a
/// [`PairsStrategy<Symbol>`](crate::strategies::PairsStrategy) whose signals
/// and levels came from runtime-typed [`AnyChain`]s.
///
/// Implements [`Strategy`] by delegation, so it drops into
/// [`crate::backtest::run`] unchanged.
pub struct DynPairsStrategy {
    inner: PairsStrategy<Symbol>,
}

impl DynPairsStrategy {
    /// Grid-wide readiness — pass-through to
    /// [`PairsStrategy::stable_bars`]. All chains are held eagerly, so
    /// this reads directly (no lazy-probe needed like basket / multi).
    pub fn stable_bars(&self) -> usize {
        self.inner.stable_bars()
    }

    /// Warm-up-only readiness (ignoring IIR settling) — pass-through to
    /// [`PairsStrategy::warm_up_bars`]. Used by
    /// `optimize --walkforward --keep-unstable`.
    pub fn warm_up_bars(&self) -> usize {
        self.inner.warm_up_bars()
    }

    /// The strategy's shared [`Book<Symbol>`] — hand-off point for
    /// portfolio-level weight-share templates that want to read
    /// `!drawdown` / `!return_per_bar` / `!trade_return` against this
    /// child's aggregate two-leg book.
    pub fn book(&self) -> Book<Symbol> {
        self.inner.book()
    }

    /// Serialize the wrapped pair's runtime state for run resuming.
    pub fn save_state(&self) -> serde_json::Value {
        self.inner.save_state()
    }

    /// Restore state produced by [`save_state`](Self::save_state).
    pub fn restore_state(&mut self, state: &serde_json::Value) -> Result<(), String> {
        self.inner.restore_state(state)
    }
}

impl Strategy for DynPairsStrategy {
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
        let spec =
            PairsStrategySpec::from_text_with_params(yaml, &std::collections::HashMap::new())
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
        let spec =
            PairsStrategySpec::from_text_with_params(yaml, &std::collections::HashMap::new())
                .unwrap();
        assert!(spec.exit.is_none() && spec.stop_loss.is_none() && spec.take_profit.is_none());
        let _built = spec.build(1_000.0, &Schema::empty());
    }

    #[test]
    fn parses_both_spread_directions() {
        let yaml = r#"
            left: BTC
            right: ETH
            long_spread:
              enter: !below { source: &z !zscore { period: 20, source: &spread !sub {
                        lhs: !close { source: !pick { symbol: BTC } },
                        rhs: !close { source: !pick { symbol: ETH } } } }, level: -2.0 }
              exit:  !above { source: *z, level: 0.0 }
              stop_loss: !sub { lhs: !sma { period: 20, source: *spread }, rhs: !value 20.0 }
            short_spread:
              enter: !above { source: *z, level: 2.0 }
              exit:  !below { source: *z, level: 0.0 }
              stop_loss: !add { lhs: !sma { period: 20, source: *spread }, rhs: !value 20.0 }
        "#;
        let spec =
            PairsStrategySpec::from_text_with_params(yaml, &std::collections::HashMap::new())
                .unwrap();
        assert!(spec.long_spread.is_some());
        assert!(spec.short_spread.is_some());
        assert!(
            spec.enter.is_none(),
            "flat keys unused when blocks are given"
        );
        let _built = spec.build(10_000.0, &Schema::empty());
    }

    #[test]
    fn a_short_spread_only_spec_needs_no_long_side() {
        let yaml = r#"
            left: BTC
            right: ETH
            short_spread:
              enter: !value true
        "#;
        let spec =
            PairsStrategySpec::from_text_with_params(yaml, &std::collections::HashMap::new())
                .unwrap();
        let _built = spec.build(1_000.0, &Schema::empty());
    }

    #[test]
    fn rejects_a_spec_that_sets_both_the_flat_keys_and_a_long_spread_block() {
        // Rejected at *parse*, not at build — so `fugazi check strategy`,
        // which never builds, catches it too.
        let yaml = r#"
            left: BTC
            right: ETH
            enter: !value true
            long_spread:
              enter: !value true
        "#;
        let err = PairsStrategySpec::from_text_with_params(yaml, &std::collections::HashMap::new())
            .expect_err("both spellings of the long side should be rejected");
        assert!(
            format!("{err:#}").contains("two spellings of the same side"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn rejects_a_spec_with_no_direction_wired() {
        let yaml = r#"
            left: BTC
            right: ETH
            sizing: !value 1.0
        "#;
        let err = PairsStrategySpec::from_text_with_params(yaml, &std::collections::HashMap::new())
            .expect_err("a pair that can never trade should be rejected");
        assert!(
            format!("{err:#}").contains("wires neither direction"),
            "unexpected error: {err:#}"
        );
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
        let spec =
            PairsStrategySpec::from_text_with_params(yaml, &std::collections::HashMap::new())
                .unwrap();
        assert!(spec.sizing.is_some());
        let _built = spec.build(10_000.0, &Schema::empty());
    }

    // -----------------------------------------------------------------
    // A pair holds two assets and privileges neither, so a leaf with no
    // `source:` has nothing to read. That used to build a sole-atom
    // `Pick` and panic on the first 2-entry snapshot — after `check` had
    // passed the document, so there was no way to find out ahead of the
    // run. It is a build error now. See `spec::expr::Root`.
    // -----------------------------------------------------------------

    /// The reported case. `!vol_target` reads prices, but looks like a
    /// scalar knob, so nothing about the document suggests it needs an
    /// asset named.
    #[test]
    fn vol_target_sizing_is_rejected_rather_than_panicking() {
        let spec: PairsStrategySpec = serde_norway::from_str(
            r#"
left: BTCUSDT
right: ETHUSDT
long_spread:
  enter: !gt { lhs: !close { source: !pick { symbol: BTCUSDT } }, rhs: !value 0.0 }
  exit: !lt { lhs: !close { source: !pick { symbol: BTCUSDT } }, rhs: !value 0.0 }
sizing: !vol_target { target: 0.20, window: 30, bars_per_year: 365 }
"#,
        )
        .unwrap();
        let error = spec
            .try_build(10_000.0, &Schema::empty())
            .err()
            .expect("a rootless price leaf in a pair has no asset to read");
        assert!(error.contains("ambiguous"), "{error}");
        assert!(error.contains("pairs"), "{error}");
        // The breadcrumb has to name the offending tag, or the message is
        // unactionable in a document with several sizing knobs.
        assert!(error.contains("!vol_target"), "{error}");
    }

    /// Not sizing-specific: any bare price leaf in a pair is unanswerable,
    /// and the breadcrumb walks down to the exact one.
    #[test]
    fn a_bare_price_leaf_in_a_signal_is_rejected_too() {
        let spec: PairsStrategySpec = serde_norway::from_str(
            r#"
left: BTCUSDT
right: ETHUSDT
long_spread:
  enter: !gt { lhs: !close, rhs: !value 0.0 }
  exit: !lt { lhs: !close, rhs: !value 0.0 }
"#,
        )
        .unwrap();
        let error = spec
            .try_build(10_000.0, &Schema::empty())
            .err()
            .expect("bare !close in a pair");
        assert!(error.contains("!gt > !close"), "{error}");
    }

    /// The fix must not reject the documents that already worked: a leaf
    /// that names its asset is exactly how a pair is supposed to read one.
    #[test]
    fn naming_the_asset_still_builds() {
        let spec: PairsStrategySpec = serde_norway::from_str(
            r#"
left: BTCUSDT
right: ETHUSDT
long_spread:
  enter: !gt { lhs: !close { source: !pick { symbol: BTCUSDT } }, rhs: !value 0.0 }
  exit: !lt { lhs: !close { source: !pick { symbol: BTCUSDT } }, rhs: !value 0.0 }
sizing: !vol_target
  source: !pick { symbol: BTCUSDT }
  target: 0.20
  window: 30
  bars_per_year: 365
"#,
        )
        .unwrap();
        spec.try_build(10_000.0, &Schema::empty())
            .expect("a leaf that names its asset is unambiguous");
    }

    /// Book-anchored sizing reads the strategy's book, not a price, so it
    /// never needed an asset and must keep building.
    #[test]
    fn book_anchored_sizing_needs_no_asset() {
        let spec: PairsStrategySpec = serde_norway::from_str(
            r#"
left: BTCUSDT
right: ETHUSDT
long_spread:
  enter: !gt { lhs: !close { source: !pick { symbol: BTCUSDT } }, rhs: !value 0.0 }
  exit: !lt { lhs: !close { source: !pick { symbol: BTCUSDT } }, rhs: !value 0.0 }
sizing: !drawdown_throttle { max_drawdown: 0.20 }
"#,
        )
        .unwrap();
        spec.try_build(10_000.0, &Schema::empty())
            .expect("a book leaf reads no asset");
    }
}
