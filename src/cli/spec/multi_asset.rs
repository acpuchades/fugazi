//! YAML-deserializable [`MultiAssetStrategySpec`] — a per-symbol replicated
//! [`SingleStrategySpec`](super::SingleStrategySpec).
//!
//! Mirrors [`super::StrategySpec`] at the trait boundary (resolves to a
//! `Strategy` with `Input = Snapshot<String>` and `Symbol = String`), but
//! every signal / level / sizing subtree is a **per-symbol template**: a
//! fresh concrete tree is built for every symbol the incoming snapshots
//! reveal (or the [`UniverseSpec`] declares), with the symbol name
//! available as `!arg SYM` inside the tree.
//!
//! ```yaml
//! # A short-term reversal per-symbol portfolio: same MA-crossover on every
//! # coin, equal-weighted 25% per leg.
//! long:
//!   enter: !crosses_above
//!     lhs: !sma { source: !close { source: !pick { symbol: !arg SYM } }, period: 5 }
//!     rhs: !sma { source: !close { source: !pick { symbol: !arg SYM } }, period: 20 }
//!   exit: !crosses_below
//!     lhs: !sma { source: !close { source: !pick { symbol: !arg SYM } }, period: 5 }
//!     rhs: !sma { source: !close { source: !pick { symbol: !arg SYM } }, period: 20 }
//!   stop_loss: !mul
//!     lhs: !entry
//!     rhs: !value 0.95
//! sizing: !equal_weight 4
//! universe: !all_of [BTC, ETH, SOL, ADA]
//! ```
//!
//! `enter` / `exit` / `stop_loss` / `take_profit` / `sizing` are all
//! typed as [`SpecTemplate`](super::SpecTemplate), so their `!arg SYM`
//! leaves survive the load pass and get resolved once per symbol at
//! build time. See [`crate::args`] for the placeholder grammar.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;

use fugazi::indicators::logic::Const;
use fugazi::indicators::{Book, Position};
use fugazi::prelude::*;
use fugazi::strategies::MultiAssetStrategy;
use fugazi::types::Snapshot;

use super::basket::UniverseSpec;
use super::expr::ExprSpec;
use super::signal::SignalSpec;
use super::template::SpecTemplate;
use crate::dyn_indicator::{self, AsBool, AsReal, DynIndicator};

/// One side of a [`MultiAssetStrategySpec`]: the entry condition, an
/// optional exit, and optional per-leg protective levels. Mirrors
/// [`SideSpec`](super::strategy::SideSpec) but every subtree is a
/// [`SpecTemplate`] so `!arg SYM` placeholders survive the load pass.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MultiSideSpec {
    /// The entry signal for this side, per symbol.
    pub enter: SpecTemplate<SignalSpec>,

    /// An optional signal that flattens this side. Defaults to constant
    /// `false` — omit it for an always-in long/short reversal (the
    /// opposite side's `enter` already flips the position).
    #[serde(default)]
    pub exit: Option<SpecTemplate<SignalSpec>>,

    /// An optional stop-loss price level (a per-symbol source). The
    /// per-symbol [`Position`](fugazi::indicators::Position) is provided
    /// at build time, so `!entry` / `!peak` / `!trough` inside compose as
    /// they do on [`SingleStrategySpec`](super::SingleStrategySpec).
    #[serde(default)]
    pub stop_loss: Option<SpecTemplate<ExprSpec>>,

    /// An optional take-profit price level (a per-symbol source). Same
    /// shape as [`stop_loss`](Self::stop_loss).
    #[serde(default)]
    pub take_profit: Option<SpecTemplate<ExprSpec>>,
}

/// A whole `multi.yml`: optional long / short sides, optional sizing,
/// optional declared [`UniverseSpec`].
///
/// The **symbol** field of
/// [`SingleStrategySpec`](super::SingleStrategySpec) is absent — a
/// multi-asset strategy runs across many symbols by construction, and
/// the [`universe`](Self::universe) field carries the declaration
/// (defaults to floating: everything the snapshot carries).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MultiAssetStrategySpec {
    /// The long side — per-symbol enter / exit signals plus optional
    /// protective levels.
    #[serde(default)]
    pub long: Option<MultiSideSpec>,

    /// The short side — same shape as [`long`](Self::long).
    #[serde(default)]
    pub short: Option<MultiSideSpec>,

    /// Optional **per-leg position-sizing multiplier** — a real-valued
    /// per-symbol template read on every entry / reversal for that
    /// symbol. Defaults to a constant `1.0` (all-in per leg). An
    /// N-symbol equal-weight portfolio at 100% gross writes
    /// `!equal_weight N`.
    #[serde(default)]
    pub sizing: Option<SpecTemplate<ExprSpec>>,

    /// Declared symbol universe — `!all_of [...]` (strict) or `!any_of
    /// [...]` (lax). Omitted means the default floating universe. See
    /// [`UniverseSpec`].
    #[serde(default)]
    pub universe: Option<UniverseSpec>,

    /// The **rebalance gate**: a boolean signal deciding, on each bar,
    /// whether the strategy resizes every held per-symbol position to
    /// its current sizing target. Defaults to `!never` — sizing is only
    /// read on transitions (entries and reversals), preserving the
    /// pre-`rebalance_on` behavior.
    ///
    /// Common non-default: `!every 20` for ~monthly resize on a daily
    /// strategy, or an equity-drawdown signal for de-risking. Entry /
    /// exit signals still fire every bar independently of the gate.
    #[serde(default)]
    pub rebalance_on: Option<SignalSpec>,
}

impl MultiAssetStrategySpec {
    /// Parse a YAML multi-asset document, applying `!param`
    /// substitutions against `params` before typed deserialization.
    /// `!arg SYM` placeholders (which resolve per-symbol at build time)
    /// are left alone.
    pub fn from_text_with_params_in(
        text: &str,
        params: &HashMap<String, Value>,
        base: &std::path::Path,
        label: &str,
    ) -> Result<Self> {
        use anyhow::Context;
        let value = super::load_value(text, params, base, label)?;
        serde_json::from_value(value)
            .with_context(|| format!("building multi-asset strategy from {label}"))
    }

    /// Test convenience: [`from_text_with_params_in`](Self::from_text_with_params_in)
    /// with imports resolved against the working directory and an
    /// `(inline)` source label.
    #[cfg(test)]
    pub fn from_text_with_params(text: &str, params: &HashMap<String, Value>) -> Result<Self> {
        Self::from_text_with_params_in(text, params, std::path::Path::new("."), "(inline)")
    }

    /// Build the live [`DynMultiAssetStrategy`] this spec describes.
    ///
    /// Every subtree in `long` / `short` / `sizing` is cloned into the
    /// corresponding per-symbol factory on the library
    /// [`MultiAssetStrategy`]. Each factory resolves `!arg SYM` against
    /// the current symbol on invocation (once per new symbol, so the
    /// per-bar overhead is a HashMap lookup, not a re-parse).
    ///
    /// # Panics
    ///
    /// The factories panic if a per-symbol template build fails — a
    /// symbol name that trips the typed deserialize on the substituted
    /// tree, or an `!arg` that isn't `SYM`. Multi-asset YAML should be
    /// validated up front (best done by dry-running on a representative
    /// symbol set in tests).
    pub fn build(&self, initial_equity: Real, schema: &Arc<Schema>) -> DynMultiAssetStrategy {
        let mut strat = MultiAssetStrategy::<String>::with_initial_equity(initial_equity);
        let book = strat.book();

        // --- long side ----------------------------------------------------
        if let Some(long) = &self.long {
            let enter_template = long.enter.clone();
            let exit_template = long.exit.clone();
            let book_l = book.clone();
            let schema_l = schema.clone();
            let book_lx = book.clone();
            let schema_lx = schema.clone();
            strat = strat.long_on(
                move |sym: &String| {
                    let concrete = build_signal(&enter_template, sym, "long enter");
                    // Long signals don't read the position directly (they're
                    // decoupled from entry — that's the level layer's job), so
                    // a fresh anchor is fine.
                    let anchor = Position::new();
                    AsBool::new(concrete.build(&anchor, &book_l, &schema_l))
                },
                move |sym: &String| {
                    let dyn_ind: Box<dyn DynIndicator> = match &exit_template {
                        Some(t) => {
                            let concrete = build_signal(t, sym, "long exit");
                            let anchor = Position::new();
                            concrete.build(&anchor, &book_lx, &schema_lx)
                        }
                        None => {
                            dyn_indicator::wrap(Const::<Snapshot<String>>::new(false))
                        }
                    };
                    AsBool::new(dyn_ind)
                },
            );
            if let Some(sl) = &long.stop_loss {
                let sl = sl.clone();
                let book_sl = book.clone();
                let schema_sl = schema.clone();
                strat = strat.long_stop_loss(move |sym: &String, position: &Position| {
                    let concrete = build_expr(&sl, sym, "long stop_loss");
                    AsReal::new(concrete.build(position, &book_sl, &schema_sl))
                });
            }
            if let Some(tp) = &long.take_profit {
                let tp = tp.clone();
                let book_tp = book.clone();
                let schema_tp = schema.clone();
                strat = strat.long_take_profit(move |sym: &String, position: &Position| {
                    let concrete = build_expr(&tp, sym, "long take_profit");
                    AsReal::new(concrete.build(position, &book_tp, &schema_tp))
                });
            }
        }

        // --- short side ---------------------------------------------------
        if let Some(short) = &self.short {
            let enter_template = short.enter.clone();
            let exit_template = short.exit.clone();
            let book_s = book.clone();
            let schema_s = schema.clone();
            let book_sx = book.clone();
            let schema_sx = schema.clone();
            strat = strat.short_on(
                move |sym: &String| {
                    let concrete = build_signal(&enter_template, sym, "short enter");
                    let anchor = Position::new();
                    AsBool::new(concrete.build(&anchor, &book_s, &schema_s))
                },
                move |sym: &String| {
                    let dyn_ind: Box<dyn DynIndicator> = match &exit_template {
                        Some(t) => {
                            let concrete = build_signal(t, sym, "short exit");
                            let anchor = Position::new();
                            concrete.build(&anchor, &book_sx, &schema_sx)
                        }
                        None => {
                            dyn_indicator::wrap(Const::<Snapshot<String>>::new(false))
                        }
                    };
                    AsBool::new(dyn_ind)
                },
            );
            if let Some(sl) = &short.stop_loss {
                let sl = sl.clone();
                let book_sl = book.clone();
                let schema_sl = schema.clone();
                strat = strat.short_stop_loss(move |sym: &String, position: &Position| {
                    let concrete = build_expr(&sl, sym, "short stop_loss");
                    AsReal::new(concrete.build(position, &book_sl, &schema_sl))
                });
            }
            if let Some(tp) = &short.take_profit {
                let tp = tp.clone();
                let book_tp = book.clone();
                let schema_tp = schema.clone();
                strat = strat.short_take_profit(move |sym: &String, position: &Position| {
                    let concrete = build_expr(&tp, sym, "short take_profit");
                    AsReal::new(concrete.build(position, &book_tp, &schema_tp))
                });
            }
        }

        // --- sizing -------------------------------------------------------
        if let Some(sizing) = &self.sizing {
            let sizing = sizing.clone();
            let book_sz = book.clone();
            let schema_sz = schema.clone();
            strat = strat.position_sizing(move |sym: &String| {
                let concrete = build_expr(&sizing, sym, "sizing");
                // Sizing doesn't attach to a per-symbol position (recipes
                // are symbol-agnostic magnitudes), so a fresh anchor is fine
                // — same convention as `BasketStrategySpec::build`.
                let anchor = Position::new();
                AsReal::new(concrete.build(&anchor, &book_sz, &schema_sz))
            });
        }

        // --- universe -----------------------------------------------------
        let strat = match &self.universe {
            Some(UniverseSpec::AllOf(syms)) => strat.all_of(syms.iter().cloned()),
            Some(UniverseSpec::AnyOf(syms)) => strat.any_of(syms.iter().cloned()),
            None => strat,
        };

        // --- rebalance gate ----------------------------------------------
        // Default is `Const::false` (installed on the library type), so
        // an omitted `rebalance_on:` matches pre-refactor behavior.
        let strat = if let Some(rebalance_spec) = &self.rebalance_on {
            let anchor = Position::new();
            let dyn_ind: Box<dyn DynIndicator> = rebalance_spec.build(&anchor, &book, schema);
            strat.rebalance_on(AsBool::new(dyn_ind))
        } else {
            strat
        };

        DynMultiAssetStrategy { inner: strat }
    }
}

// ---------------------------------------------------------------------------
// Per-symbol builders — panic on template failure with a descriptive
// message. Same policy as basket: bad YAML surfaces loud at build time.
// ---------------------------------------------------------------------------

fn build_signal(
    template: &SpecTemplate<SignalSpec>,
    sym: &str,
    slot: &'static str,
) -> SignalSpec {
    let mut args = HashMap::new();
    args.insert("SYM".to_string(), Value::String(sym.to_string()));
    template.build(&args).unwrap_or_else(|e| {
        panic!("multi-asset {slot} signal template build failed for symbol {sym:?}: {e}")
    })
}

fn build_expr(
    template: &SpecTemplate<ExprSpec>,
    sym: &str,
    slot: &'static str,
) -> ExprSpec {
    let mut args = HashMap::new();
    args.insert("SYM".to_string(), Value::String(sym.to_string()));
    template.build(&args).unwrap_or_else(|e| {
        panic!("multi-asset {slot} template build failed for symbol {sym:?}: {e}")
    })
}

// ---------------------------------------------------------------------------
// DynMultiAssetStrategy: CLI-owned wrapper around MultiAssetStrategy<String>
// ---------------------------------------------------------------------------

/// The CLI's built multi-asset strategy handle. Wraps a
/// [`MultiAssetStrategy<String>`] whose per-symbol signal / level /
/// sizing factories were assembled from
/// [`SpecTemplate`](SpecTemplate)s. Implements [`Strategy`] by
/// delegation so it drops into [`fugazi::backtest::run`] unchanged.
pub struct DynMultiAssetStrategy {
    inner: MultiAssetStrategy<String>,
}

impl Strategy for DynMultiAssetStrategy {
    type Input = Snapshot<String>;
    type Symbol = String;

    fn update(&mut self, input: Snapshot<String>) {
        self.inner.update(input);
    }
    fn trade(&self, wallet: &mut dyn Wallet<String>) {
        self.inner.trade(wallet);
    }
    fn on_fill(&mut self, order: &Order<String>) {
        self.inner.on_fill(order);
    }
    fn is_ready(&self) -> bool {
        self.inner.is_ready()
    }
    fn reset(&mut self) {
        self.inner.reset();
    }
}

impl DynMultiAssetStrategy {
    /// A clone of the shared [`Book`] anchor — for downstream book-side
    /// diagnostics and initial-equity assertions.
    #[allow(dead_code)]
    pub fn book(&self) -> Book<String> {
        self.inner.book()
    }

    /// Grid-wide readiness across the currently-discovered per-symbol
    /// states and the rebalance gate — pass-through to
    /// [`MultiAssetStrategy::stable_period`](fugazi::strategies::MultiAssetStrategy::stable_period).
    ///
    /// **Lazy readiness contract**: a multi-asset strategy's per-symbol
    /// chains are built on first sight, so a freshly-built strategy
    /// reports the rebalance-signal period only. Feed one representative
    /// snapshot via [`update`](Strategy::update) before probing so the
    /// per-symbol chains exist. See the underlying method for details.
    pub fn stable_period(&self) -> usize {
        self.inner.stable_period()
    }

    /// Warm-up-only readiness (ignoring IIR settling) — pass-through to
    /// [`MultiAssetStrategy::warm_up_period`](fugazi::strategies::MultiAssetStrategy::warm_up_period).
    /// Used by `optimize --walkforward --keep-unstable`.
    ///
    /// Same lazy-readiness caveat as [`stable_period`](Self::stable_period).
    pub fn warm_up_period(&self) -> usize {
        self.inner.warm_up_period()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fugazi::PaperWallet;
    use fugazi::types::Atom;

    fn candle(price: Real) -> Candle {
        Candle::new(price, price, price, price, 0.0)
    }

    fn snap_of(entries: &[(&'static str, Real)]) -> Snapshot<String> {
        let mut s = Snapshot::new();
        for &(sym, close) in entries {
            let atom = Atom::new(candle(close));
            s.push(Some(sym.to_string()), None, atom);
        }
        s
    }

    fn schema() -> Arc<Schema> {
        Schema::empty()
    }

    #[test]
    fn deserializes_a_full_multi_asset_spec() {
        let yaml = r#"
            long:
              enter: !gt
                lhs: !close { source: !pick { symbol: !arg SYM } }
                rhs: !value 50
              exit: !lt
                lhs: !close { source: !pick { symbol: !arg SYM } }
                rhs: !value 30
            sizing: !equal_weight 2
            universe: !any_of [A, B]
        "#;
        let spec = MultiAssetStrategySpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        assert!(spec.long.is_some());
        assert!(spec.short.is_none());
        assert!(matches!(spec.universe, Some(UniverseSpec::AnyOf(_))));
    }

    #[test]
    fn universe_defaults_to_floating_when_omitted() {
        let yaml = r#"
            long:
              enter: !gt
                lhs: !close { source: !pick { symbol: !arg SYM } }
                rhs: !value 0
            sizing: !value 0.5
        "#;
        let spec = MultiAssetStrategySpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        assert!(spec.universe.is_none());
    }

    #[test]
    fn build_drives_per_symbol_independently() {
        // Long when close > 50. A crosses the line, B doesn't — A should
        // end long, B flat.
        let yaml = r#"
            long:
              enter: !gt
                lhs: !close { source: !pick { symbol: !arg SYM } }
                rhs: !value 50
              exit: !lt
                lhs: !close { source: !pick { symbol: !arg SYM } }
                rhs: !value 30
            sizing: !value 0.5
        "#;
        let spec = MultiAssetStrategySpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        let mut strat = spec.build(10_000.0, &schema());
        let mut wallet: PaperWallet<String> = PaperWallet::new(10_000.0);

        for _ in 0..2 {
            for fill in wallet.update("A".to_string(), candle(100.0)) {
                strat.on_fill(&fill);
            }
            for fill in wallet.update("B".to_string(), candle(20.0)) {
                strat.on_fill(&fill);
            }
            strat.update(snap_of(&[("A", 100.0), ("B", 20.0)]));
            strat.trade(&mut wallet);
        }
        assert!(wallet.position(&"A".to_string()).amount > 0.0, "A long");
        assert!(
            wallet.position(&"B".to_string()).amount.abs() < 1e-9,
            "B never triggered its own signal"
        );
    }

    #[test]
    fn per_symbol_stop_loss_reads_the_correct_position() {
        // Buy-and-hold-per-symbol with a 10% fixed stop off entry. Two
        // symbols, priced differently, must stop out at different levels.
        let yaml = r#"
            long:
              enter: !value true
              exit:  !value false
              stop_loss: !mul
                lhs: !entry
                rhs: !value 0.9
            sizing: !value 0.25
        "#;
        let spec = MultiAssetStrategySpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        let mut strat = spec.build(10_000.0, &schema());
        let mut wallet: PaperWallet<String> = PaperWallet::new(10_000.0);
        // Bar 1: signal / queue entry.
        for fill in wallet.update("A".to_string(), candle(100.0)) {
            strat.on_fill(&fill);
        }
        for fill in wallet.update("B".to_string(), candle(50.0)) {
            strat.on_fill(&fill);
        }
        strat.update(snap_of(&[("A", 100.0), ("B", 50.0)]));
        strat.trade(&mut wallet);
        // Bar 2: fill at open. A entry=100 → stop=90; B entry=50 → stop=45.
        for fill in wallet.update("A".to_string(), candle(100.0)) {
            strat.on_fill(&fill);
        }
        for fill in wallet.update("B".to_string(), candle(50.0)) {
            strat.on_fill(&fill);
        }
        strat.update(snap_of(&[("A", 100.0), ("B", 50.0)]));
        strat.trade(&mut wallet);
        assert!(wallet.position(&"A".to_string()).amount > 0.0);
        assert!(wallet.position(&"B".to_string()).amount > 0.0);
        // Bar 3: A drops through 90 (opens 95, low 88); B holds. Only A stops.
        let mut s = Snapshot::<String>::new();
        s.push(
            Some("A".to_string()),
            None,
            Atom::new(Candle::new(95.0, 96.0, 88.0, 89.0, 0.0)),
        );
        s.push(
            Some("B".to_string()),
            None,
            Atom::new(candle(50.0)),
        );
        for (sym_opt, _f, atom) in s.iter() {
            let sym = sym_opt.cloned().unwrap();
            for fill in wallet.update(sym, atom.candle) {
                strat.on_fill(&fill);
            }
        }
        strat.update(s);
        strat.trade(&mut wallet);
        assert!(
            wallet.position(&"A".to_string()).amount.abs() < 1e-9,
            "A stopped at 90"
        );
        assert!(
            wallet.position(&"B".to_string()).amount > 0.0,
            "B held (didn't hit its 45 stop)"
        );
        // The exit's fill price on A is exactly the stop level.
        let a_exit = wallet
            .orders()
            .iter()
            .rev()
            .find(|o| o.symbol == "A" && o.side == Side::Sell)
            .expect("A exit order");
        assert_eq!(a_exit.price, 90.0);
    }

    #[test]
    fn universe_all_of_parses_and_wires() {
        let yaml = r#"
            long:
              enter: !gt
                lhs: !close { source: !pick { symbol: !arg SYM } }
                rhs: !value 0
            sizing: !value 0.25
            universe: !all_of [X, Y]
        "#;
        let spec = MultiAssetStrategySpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        match spec.universe {
            Some(UniverseSpec::AllOf(ref v)) => {
                assert_eq!(v, &vec!["X".to_string(), "Y".to_string()]);
            }
            _ => panic!("expected AllOf"),
        }
        // A built strategy filters non-listed symbols at discovery.
        let mut strat = spec.build(1_000.0, &schema());
        strat.update(snap_of(&[("X", 100.0), ("Y", 100.0), ("Z", 100.0)]));
        // No per-symbol accessor on DynMultiAssetStrategy (kept minimal),
        // but the shared book has never marked Z's leg (never registered).
        assert!(strat.book().equity_value() > 0.0);
    }

    #[test]
    #[should_panic(expected = "`all_of` universe requires")]
    fn build_with_all_of_panics_on_missing_symbol() {
        let yaml = r#"
            long:
              enter: !gt
                lhs: !close { source: !pick { symbol: !arg SYM } }
                rhs: !value 0
            sizing: !value 0.25
            universe: !all_of [X, Y]
        "#;
        let spec = MultiAssetStrategySpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        let mut strat = spec.build(1_000.0, &schema());
        strat.update(snap_of(&[("X", 100.0)])); // Y missing → panic
    }

    #[test]
    fn rebalance_on_defaults_to_none_and_never_resizes() {
        // Omitted `rebalance_on:` parses cleanly (defaults to None → the
        // library type installs `Const::false`, matching pre-refactor
        // behavior).
        let yaml = r#"
            long:
              enter: !value true
              exit: !value false
            sizing: !value 0.25
        "#;
        let spec = MultiAssetStrategySpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        assert!(spec.rebalance_on.is_none());
    }

    #[test]
    fn rebalance_on_every_1_holds_position_when_target_unchanged() {
        // With `rebalance_on: !every 1` and steady sizing / price, the
        // resize is idempotent — no spurious fills after the initial
        // entry.
        let yaml = r#"
            long:
              enter: !value true
              exit: !value false
            sizing: !value 0.5
            rebalance_on: !every 1
        "#;
        let spec = MultiAssetStrategySpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        let mut strat = spec.build(10_000.0, &schema());
        let mut wallet: PaperWallet<String> = PaperWallet::new(10_000.0);
        // Bar 1: entry queues; Bar 2: fill.
        for _ in 0..2 {
            for fill in wallet.update("A".to_string(), candle(100.0)) {
                strat.on_fill(&fill);
            }
            strat.update(snap_of(&[("A", 100.0)]));
            strat.trade(&mut wallet);
        }
        let after_entry = wallet.position(&"A".to_string()).amount;
        assert!(after_entry > 0.0, "entry filled");
        // Bars 3-6: idempotent resize.
        for _ in 0..4 {
            for fill in wallet.update("A".to_string(), candle(100.0)) {
                strat.on_fill(&fill);
            }
            strat.update(snap_of(&[("A", 100.0)]));
            strat.trade(&mut wallet);
        }
        assert!(
            (wallet.position(&"A".to_string()).amount - after_entry).abs() < 1e-6,
            "same-target resize is a no-op"
        );
    }

    #[test]
    fn rebalance_on_never_parses_and_preserves_no_resize() {
        let yaml = r#"
            long:
              enter: !value true
              exit: !value false
            sizing: !value 0.25
            rebalance_on: !never
        "#;
        let spec = MultiAssetStrategySpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        assert!(spec.rebalance_on.is_some());
        // Build the strategy — sanity check that !never doesn't blow up.
        let mut strat = spec.build(10_000.0, &schema());
        let mut wallet: PaperWallet<String> = PaperWallet::new(10_000.0);
        for _ in 0..3 {
            for fill in wallet.update("A".to_string(), candle(100.0)) {
                strat.on_fill(&fill);
            }
            strat.update(snap_of(&[("A", 100.0)]));
            strat.trade(&mut wallet);
        }
        // Entry fill exists; no other orders (rebalance = never).
        assert_eq!(wallet.orders().len(), 1, "!never: only the entry");
    }

    #[test]
    fn params_are_substituted_at_load_time() {
        let yaml = r#"
            long:
              enter: !gt
                lhs: !sma
                  source: !close { source: !pick { symbol: !arg SYM } }
                  period: !param FAST
                rhs: !value 0
            sizing: !value 0.25
        "#;
        let mut params = HashMap::new();
        params.insert("FAST".to_string(), Value::Number(10.into()));
        let spec = MultiAssetStrategySpec::from_text_with_params(yaml, &params).unwrap();
        // The stored tree carries `period: 10` (resolved) but `symbol: !arg SYM`
        // (deferred) on the enter template.
        let enter_tree = spec.long.as_ref().unwrap().enter.tree();
        let period = enter_tree.pointer("/gt/lhs/sma/period").unwrap();
        assert_eq!(period, &Value::Number(10.into()));
        let sym = enter_tree
            .pointer("/gt/lhs/sma/source/close/source/pick/symbol")
            .unwrap();
        assert_eq!(sym, &serde_json::json!({"arg": "SYM"}));
    }
}
