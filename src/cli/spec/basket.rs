//! YAML-deserializable [`BasketStrategySpec`] — a cross-sectional N-symbol
//! basket strategy.
//!
//! Mirrors [`super::StrategySpec`] and [`super::PairsStrategySpec`] at the
//! trait boundary (both resolve to a `Strategy` with `Input =
//! Snapshot<String>` and `Symbol = String`), but the score and sizing
//! sources are **per-symbol templates**: they get a fresh
//! [`ExprSpec`] built for every symbol the incoming snapshots reveal, with
//! the symbol name available as `!arg SYM` inside the tree.
//!
//! ```yaml
//! selection: !top_bottom { longs: 3, shorts: 3 }
//! score:
//!   !mul
//!     lhs: !roc { source: !close { source: !pick { symbol: !arg SYM } }, periods: 20 }
//!     rhs: !adx { source: !current { source: !pick { symbol: !arg SYM } }, period: 14 }
//! sizing: !equal_weight 6
//! ```
//!
//! Both `score` and `sizing` are typed as
//! [`SpecTemplate<ExprSpec>`](super::SpecTemplate), so a `!arg SYM` leaf
//! survives the load pass and gets resolved once per symbol at build
//! time. See [`crate::args`] for the placeholder grammar.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;

use fugazi::indicators::{Book, Position};
use fugazi::prelude::*;
use fugazi::strategies::BasketStrategy;
use fugazi::types::Snapshot;

use super::expr::ExprSpec;
use super::template::SpecTemplate;
use crate::dyn_indicator::{AsReal, DynIndicator};

/// YAML surface for the ranking rule. Externally tagged
/// (`!top_bottom { longs, shorts }` / `!threshold { long_min, short_max }`
/// / `!quantile { long_q, short_q }`).
///
/// Kept separate from [`fugazi::strategies::SelectionRule`] because the
/// library's enum isn't `Deserialize`; this spec is the CLI-only
/// discriminator that dispatches to the three free functions
/// `top_bottom` / `threshold` / `quantile` at build time.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum SelectionRuleSpec {
    /// Take the `longs` highest-scoring symbols long, the `shorts`
    /// lowest-scoring short. See
    /// [`fugazi::strategies::basket::top_bottom`].
    TopBottom { longs: usize, shorts: usize },

    /// Long every symbol scoring at/above `long_min`; short at/below
    /// `short_max`. See [`fugazi::strategies::basket::threshold`].
    Threshold { long_min: Real, short_max: Real },

    /// Long the top `long_q` fraction of the score distribution; short
    /// the bottom `short_q`. See
    /// [`fugazi::strategies::basket::quantile`].
    Quantile { long_q: Real, short_q: Real },
}

/// YAML surface for a declared basket [`Universe`](fugazi::strategies::basket::Universe).
///
/// Externally tagged, taking a raw list of symbol names:
///
/// ```yaml
/// universe: !all_of [BTC, ETH, SOL]     # strict: panic on absence, wait for all
/// universe: !any_of [BTC, ETH, SOL]     # lax:    silently skip absent / unready
/// ```
///
/// Omitted (`universe:` absent from the spec) means the default floating
/// universe — every symbol seen in the snapshot is picked up on first
/// sight.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum UniverseSpec {
    /// Strict declared universe: every listed symbol must be present on
    /// every bar (absence panics); readiness gates on all listed symbols
    /// scoring `Some`. See
    /// [`fugazi::strategies::basket::Universe::AllOf`].
    AllOf(Vec<String>),

    /// Lax declared universe: restrict to the listed subset but silently
    /// skip absent or still-unready members. See
    /// [`fugazi::strategies::basket::Universe::AnyOf`].
    AnyOf(Vec<String>),
}

/// A whole `basket.yml`: the ranking rule plus deferred score and sizing
/// templates, resolved per-symbol at build time.
///
/// See the module doc for the `!arg SYM` substitution convention.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BasketStrategySpec {
    /// How ranked scores turn into a per-symbol side.
    pub selection: SelectionRuleSpec,

    /// The per-symbol scoring source: a real-valued expression evaluated
    /// once per bar for every symbol in the snapshot. Written as a normal
    /// `ExprSpec` tree with `!arg SYM` placeholders where the current
    /// symbol should be substituted.
    pub score: SpecTemplate<ExprSpec>,

    /// The per-symbol sizing source: the per-leg `ValueFraction`
    /// magnitude every selected symbol is entered at. Same shape as
    /// `score` — normal `ExprSpec` with `!arg SYM` placeholders.
    ///
    /// For the equal-weight common case (100% gross across an N-symbol
    /// basket), write `!equal_weight <n_legs>` — the constant `1.0 /
    /// n_legs` per leg. No `!arg` is needed there since equal-weight
    /// doesn't depend on the symbol.
    pub sizing: SpecTemplate<ExprSpec>,

    /// Declared symbol universe — `!all_of [...]` (strict: error on
    /// absence, wait until every listed symbol is ready) or `!any_of
    /// [...]` (lax: silently skip absent / unready). Omitted means the
    /// default floating universe (every symbol seen in the snapshot is
    /// picked up on first sight). See [`UniverseSpec`].
    #[serde(default)]
    pub universe: Option<UniverseSpec>,
}

impl BasketStrategySpec {
    /// Parse a YAML basket document, applying `!param` substitutions
    /// against `params` before typed deserialization. `!arg` placeholders
    /// (which resolve per-symbol at build time) are left alone.
    pub fn from_text_with_params_in(
        text: &str,
        params: &HashMap<String, Value>,
        base: &std::path::Path,
        label: &str,
    ) -> Result<Self> {
        use anyhow::Context;
        let value = super::load_value(text, params, base, label)?;
        serde_json::from_value(value)
            .with_context(|| format!("building basket strategy from {label}"))
    }

    /// [`from_text_with_params_in`](Self::from_text_with_params_in) with imports
    /// resolved against the working directory and an `(inline)` source label —
    /// a test convenience (the CLI passes the strategy source's `base_dir()`
    /// and `label()`).
    #[cfg(test)]
    pub fn from_text_with_params(text: &str, params: &HashMap<String, Value>) -> Result<Self> {
        Self::from_text_with_params_in(text, params, std::path::Path::new("."), "(inline)")
    }

    /// Build the live [`DynBasketStrategy`] this spec describes.
    ///
    /// The score and sizing templates are cloned into the corresponding
    /// per-symbol factories on the library `BasketStrategy`. Each factory
    /// resolves `!arg SYM` against the current symbol on invocation
    /// (once per new symbol, so the per-bar overhead is a HashMap lookup,
    /// not a re-parse).
    ///
    /// # Panics
    ///
    /// The score/sizing factories panic if a per-symbol template build
    /// fails — a symbol name that trips the typed deserialize on the
    /// substituted tree, or an `!arg` that isn't `SYM`. Basket YAML
    /// should be validated up front (best done by dry-running on a
    /// representative symbol set in tests).
    ///
    /// The **per-leg `Position` accessors** (`!entry`, `!peak`, `!trough`)
    /// are wired to a *dummy* `Position` inside score/sizing subtrees, so
    /// they always read `None` in a basket. Those accessors make sense
    /// only inside a per-side protective level (a follow-up on
    /// `BasketStrategy`); using them in a score / sizing expression today
    /// silently produces `None` and skips the leg. The shared `Book`
    /// anchor *is* wired, so book-anchored sizing recipes
    /// (`!drawdown_throttle`, `!equity_vol_target`, `!fractional_kelly`)
    /// work on the basket's aggregate equity curve.
    pub fn build(&self, initial_equity: Real, schema: &Arc<Schema>) -> DynBasketStrategy {
        let strat = BasketStrategy::<String>::with_initial_equity(initial_equity);
        let book = strat.book();

        let score_template = self.score.clone();
        let book_score = book.clone();
        let schema_score = schema.clone();
        let strat = strat.scored_by(move |sym: &String| {
            let concrete = build_per_symbol(&score_template, sym, "score");
            let anchor = Position::new();
            let dyn_ind: Box<dyn DynIndicator> =
                concrete.build(&anchor, &book_score, &schema_score);
            AsReal::new(dyn_ind)
        });

        let sizing_template = self.sizing.clone();
        let book_sizing = book.clone();
        let schema_sizing = schema.clone();
        let strat = strat.sized_by(move |sym: &String| {
            let concrete = build_per_symbol(&sizing_template, sym, "sizing");
            let anchor = Position::new();
            let dyn_ind: Box<dyn DynIndicator> =
                concrete.build(&anchor, &book_sizing, &schema_sizing);
            AsReal::new(dyn_ind)
        });

        let strat = match self.selection {
            SelectionRuleSpec::TopBottom { longs, shorts } => strat.top_bottom(longs, shorts),
            SelectionRuleSpec::Threshold {
                long_min,
                short_max,
            } => strat.threshold(long_min, short_max),
            SelectionRuleSpec::Quantile { long_q, short_q } => {
                strat.quantile(long_q, short_q)
            }
        };

        let strat = match &self.universe {
            Some(UniverseSpec::AllOf(syms)) => strat.all_of(syms.iter().cloned()),
            Some(UniverseSpec::AnyOf(syms)) => strat.any_of(syms.iter().cloned()),
            None => strat,
        };

        DynBasketStrategy { inner: strat }
    }
}

/// Resolve a per-symbol template into a concrete `ExprSpec` by supplying
/// `SYM` from `sym`. Panics with a descriptive message on failure — the
/// build-time template resolution is a config error, not a runtime
/// condition to recover from, so a loud panic surfaces the bad YAML.
fn build_per_symbol(
    template: &SpecTemplate<ExprSpec>,
    sym: &str,
    slot: &'static str,
) -> ExprSpec {
    let mut args = HashMap::new();
    args.insert("SYM".to_string(), Value::String(sym.to_string()));
    template
        .build(&args)
        .unwrap_or_else(|e| panic!("basket {slot} template build failed for symbol {sym:?}: {e}"))
}

// ---------------------------------------------------------------------------
// DynBasketStrategy: CLI-owned wrapper around BasketStrategy<String>
// ---------------------------------------------------------------------------

/// The CLI's built-basket handle. Wraps a
/// [`BasketStrategy<String>`](fugazi::strategies::BasketStrategy) whose
/// per-symbol score / sizing factories were assembled from
/// [`SpecTemplate<ExprSpec>`](SpecTemplate).
///
/// Implements [`Strategy`](fugazi::Strategy) by delegation, so it drops
/// into [`fugazi::backtest::run`] unchanged (once the CLI dispatch grows
/// a `basket:` prefix — a follow-up).
pub struct DynBasketStrategy {
    inner: BasketStrategy<String>,
}

impl Strategy for DynBasketStrategy {
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

impl DynBasketStrategy {
    /// A clone of the shared [`Book`] anchor — for downstream book-side
    /// diagnostics and (once CLI dispatch grows a basket path) initial
    /// equity assertions.
    #[allow(dead_code)]
    pub fn book(&self) -> Book<String> {
        self.inner.book()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fugazi::PaperWallet;
    use fugazi::types::{Atom, Selector};

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
    fn deserializes_a_full_basket_spec() {
        let yaml = r#"
            selection: !top_bottom { longs: 2, shorts: 2 }
            score:
              !roc
                source: !close { source: !pick { symbol: !arg SYM } }
                periods: 5
            sizing: !equal_weight 4
        "#;
        let spec = BasketStrategySpec::from_text_with_params(
            yaml,
            &HashMap::new(),
        )
        .unwrap();
        match spec.selection {
            SelectionRuleSpec::TopBottom { longs, shorts } => {
                assert_eq!(longs, 2);
                assert_eq!(shorts, 2);
            }
            _ => panic!("expected TopBottom"),
        }
    }

    #[test]
    fn each_selection_variant_round_trips() {
        for (yaml, expected) in [
            (
                "!threshold { long_min: 0.5, short_max: -0.5 }",
                "threshold",
            ),
            (
                "!quantile { long_q: 0.1, short_q: 0.1 }",
                "quantile",
            ),
        ] {
            let rule: SelectionRuleSpec = serde_norway::from_str(yaml).unwrap();
            match (rule, expected) {
                (SelectionRuleSpec::Threshold { .. }, "threshold") => {}
                (SelectionRuleSpec::Quantile { .. }, "quantile") => {}
                (r, e) => panic!("unexpected variant for {yaml}: got {r:?}, expected {e}"),
            }
        }
    }

    #[test]
    fn build_produces_a_working_strategy_that_ranks_by_score() {
        // Score = close price (via !close{!pick{!arg SYM}}); rank top-1 long,
        // bottom-1 short; sized 50% ValueFraction per leg. Drive two bars —
        // bar 1 to prime, bar 2 to fill. A > C in close, so A should end
        // long and C short.
        let yaml = r#"
            selection: !top_bottom { longs: 1, shorts: 1 }
            score: !close { source: !pick { symbol: !arg SYM } }
            sizing: !value 0.5
        "#;
        let spec =
            BasketStrategySpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        let mut strat = spec.build(10_000.0, &schema());
        let mut wallet: PaperWallet<String> = PaperWallet::new(10_000.0);

        for _ in 0..2 {
            let bar_a = candle(100.0);
            let bar_b = candle(50.0);
            let bar_c = candle(25.0);
            for fill in wallet.update("A".to_string(), bar_a) {
                strat.on_fill(&fill);
            }
            for fill in wallet.update("B".to_string(), bar_b) {
                strat.on_fill(&fill);
            }
            for fill in wallet.update("C".to_string(), bar_c) {
                strat.on_fill(&fill);
            }
            strat.update(snap_of(&[("A", 100.0), ("B", 50.0), ("C", 25.0)]));
            strat.trade(&mut wallet);
        }
        assert!(
            wallet.position(&"A".to_string()).amount > 0.0,
            "A should be long"
        );
        assert!(
            wallet.position(&"C".to_string()).amount < 0.0,
            "C should be short"
        );
    }

    #[test]
    fn sym_arg_is_substituted_per_symbol_via_pick() {
        // If the `!arg SYM` weren't substituted per-symbol, every symbol's
        // score would read the same asset — likely panicking on the
        // multi-entry snapshot inside an empty-selector `Pick`. Verify the
        // per-symbol build by ensuring both symbols get their own score.
        // (A trivial constant sizing keeps the scenario simple.)
        let yaml = r#"
            selection: !top_bottom { longs: 1, shorts: 0 }
            score: !close { source: !pick { symbol: !arg SYM } }
            sizing: !value 0.25
        "#;
        let spec =
            BasketStrategySpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        let mut strat = spec.build(10_000.0, &schema());

        // Two-bar prime + fill on symbols {X, Y}; X's close > Y's, so X wins.
        let mut wallet: PaperWallet<String> = PaperWallet::new(10_000.0);
        for _ in 0..2 {
            for fill in wallet.update("X".to_string(), candle(200.0)) {
                strat.on_fill(&fill);
            }
            for fill in wallet.update("Y".to_string(), candle(100.0)) {
                strat.on_fill(&fill);
            }
            strat.update(snap_of(&[("X", 200.0), ("Y", 100.0)]));
            strat.trade(&mut wallet);
        }
        assert!(wallet.position(&"X".to_string()).amount > 0.0);
        assert!(wallet.position(&"Y".to_string()).amount.abs() < 1e-9);
        // Sanity: A separate `Selector::by_symbol("X")` `find` on the same
        // shape retrieves X's atom.
        let snap = snap_of(&[("X", 200.0), ("Y", 100.0)]);
        assert!(snap.find(&Selector::by_symbol("X".to_string())).is_some());
    }

    #[test]
    fn universe_defaults_to_floating_when_omitted() {
        let yaml = r#"
            selection: !top_bottom { longs: 1, shorts: 1 }
            score: !close { source: !pick { symbol: !arg SYM } }
            sizing: !value 0.5
        "#;
        let spec =
            BasketStrategySpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        assert!(spec.universe.is_none());
    }

    #[test]
    fn universe_all_of_parses_symbol_list() {
        let yaml = r#"
            selection: !top_bottom { longs: 1, shorts: 1 }
            score: !close { source: !pick { symbol: !arg SYM } }
            sizing: !value 0.5
            universe: !all_of [BTC, ETH, SOL]
        "#;
        let spec =
            BasketStrategySpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        match spec.universe {
            Some(UniverseSpec::AllOf(v)) => {
                assert_eq!(v, vec!["BTC".to_string(), "ETH".to_string(), "SOL".to_string()]);
            }
            other => panic!("expected AllOf, got {other:?}"),
        }
    }

    #[test]
    fn universe_any_of_parses_symbol_list() {
        let yaml = r#"
            selection: !top_bottom { longs: 1, shorts: 1 }
            score: !close { source: !pick { symbol: !arg SYM } }
            sizing: !value 0.5
            universe: !any_of [BTC, ETH]
        "#;
        let spec =
            BasketStrategySpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        match spec.universe {
            Some(UniverseSpec::AnyOf(v)) => {
                assert_eq!(v, vec!["BTC".to_string(), "ETH".to_string()]);
            }
            other => panic!("expected AnyOf, got {other:?}"),
        }
    }

    #[test]
    fn build_with_all_of_filters_non_listed_symbols() {
        // Universe = {X, Y}. Snapshot also carries Z — the built strategy
        // must ignore Z at discovery (no chain, no fill).
        let yaml = r#"
            selection: !top_bottom { longs: 1, shorts: 1 }
            score: !close { source: !pick { symbol: !arg SYM } }
            sizing: !value 0.5
            universe: !all_of [X, Y]
        "#;
        let spec =
            BasketStrategySpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        let mut strat = spec.build(10_000.0, &schema());
        let mut wallet: PaperWallet<String> = PaperWallet::new(10_000.0);

        for _ in 0..2 {
            for fill in wallet.update("X".to_string(), candle(200.0)) {
                strat.on_fill(&fill);
            }
            for fill in wallet.update("Y".to_string(), candle(100.0)) {
                strat.on_fill(&fill);
            }
            for fill in wallet.update("Z".to_string(), candle(500.0)) {
                strat.on_fill(&fill);
            }
            strat.update(snap_of(&[("X", 200.0), ("Y", 100.0), ("Z", 500.0)]));
            strat.trade(&mut wallet);
        }
        assert!(wallet.position(&"X".to_string()).amount > 0.0, "X long");
        assert!(wallet.position(&"Y".to_string()).amount < 0.0, "Y short");
        assert!(
            wallet.position(&"Z".to_string()).amount.abs() < 1e-9,
            "Z is outside the declared universe: no trade"
        );
    }

    #[test]
    #[should_panic(expected = "`all_of` universe requires")]
    fn build_with_all_of_panics_on_missing_symbol() {
        let yaml = r#"
            selection: !top_bottom { longs: 1, shorts: 1 }
            score: !close { source: !pick { symbol: !arg SYM } }
            sizing: !value 0.5
            universe: !all_of [X, Y]
        "#;
        let spec =
            BasketStrategySpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        let mut strat = spec.build(10_000.0, &schema());
        // Y is missing from the snapshot — strict-erroring.
        strat.update(snap_of(&[("X", 100.0)]));
    }

    #[test]
    fn params_are_substituted_at_load_time() {
        // `!param FAST` gets resolved from `--params`, `!arg SYM` remains
        // deferred for the per-symbol build.
        let yaml = r#"
            selection: !top_bottom { longs: 1, shorts: 1 }
            score:
              !roc
                source: !close { source: !pick { symbol: !arg SYM } }
                periods: !param FAST
            sizing: !value 0.5
        "#;
        let mut params = HashMap::new();
        params.insert("FAST".to_string(), Value::Number(10.into()));
        let spec = BasketStrategySpec::from_text_with_params(yaml, &params).unwrap();
        // The stored tree should carry `periods: 10` (resolved) and
        // `symbol: {arg: "SYM"}` (deferred).
        let tree = spec.score.tree();
        let periods = tree.pointer("/roc/periods").unwrap();
        assert_eq!(periods, &Value::Number(10.into()));
        let sym = tree.pointer("/roc/source/close/source/pick/symbol").unwrap();
        assert_eq!(sym, &serde_json::json!({"arg": "SYM"}));
    }
}
