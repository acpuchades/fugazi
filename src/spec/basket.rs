//! YAML-deserializable [`BasketStrategySpec`] — a cross-sectional N-symbol
//! basket strategy.
//!
//! Mirrors [`super::StrategySpec`] and [`super::PairsStrategySpec`] at the
//! trait boundary (both resolve to a `Strategy` with `Input =
//! Snapshot<Symbol>` and `Symbol = Symbol`), but the score and sizing
//! sources are **per-symbol templates**: they get a fresh
//! [`NodeSpec`] built for every symbol the incoming snapshots reveal, with
//! the symbol name available as `!arg SYM` inside the tree.
//!
//! ```yaml
//! selection: !top_bottom { longs: 3, shorts: 3 }
//! score:
//!   !mul
//!     lhs: !roc { source: !close { source: !pick { symbol: !arg SYM } }, period: 20 }
//!     rhs: !adx { source: !current { source: !pick { symbol: !arg SYM } }, period: 14 }
//! sizing: !equal_weight 6
//! ```
//!
//! Both `score` and `sizing` are typed as
//! [`SpecTemplate<NodeSpec>`](super::SpecTemplate), so a `!arg SYM` leaf
//! survives the load pass and gets resolved once per symbol at build
//! time. See [`crate::spec::args`] for the placeholder grammar.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use fugazi_derive::SpecGrammar;
use serde::Deserialize;
use serde_json::Value;

use crate::indicators::{Book, Position};
use crate::prelude::*;
use crate::strategies::BasketStrategy;
use crate::strategies::basket::{
    DynSelection, Everything, Quantile, Selection, Threshold, TopBottom,
};
use crate::types::Snapshot;

use super::expr::{NodeSpec, Root};
use super::meta::Meta;
use super::template::SpecTemplate;
use crate::runtime::AnyChain;
use crate::types::Symbol;

/// YAML surface for the ranking rule. Externally tagged
/// (`!top_bottom { longs, shorts }` / `!threshold { long_min, short_max }`
/// / `!quantile { long_q, short_q }` / `!everything`).
///
/// **Composable.** Every rule carries an optional `of:` inner rule that
/// defaults to [`!everything`](Self::Everything) — the full universe. A
/// bare `!top_bottom { longs, shorts }` therefore ranks every symbol,
/// while
///
/// ```yaml
/// selection: !top_bottom { longs: 2, shorts: 2, of: !threshold { long_min: 0.5, short_max: -0.5 } }
/// ```
///
/// ranks the top-2 / bottom-2 *of the threshold survivors* — the YAML
/// mirror of `TopBottom::of(Threshold::new(0.5, -0.5), 2, 2)`. Each stage
/// narrows the inner's per-side candidate sets, so the chain nests to any
/// depth.
///
/// A CLI-only discriminator; at build it constructs the corresponding
/// [`crate::strategies::basket::Selection`] chain (one of
/// [`Everything`] /
/// [`TopBottom`] /
/// [`Threshold`] /
/// [`Quantile`]) and installs it via
/// [`BasketStrategy::selection`](crate::strategies::BasketStrategy::selection).
/// Rust-side callers with a custom rule build their own `Selection`
/// impl and install it directly — no CLI-side wiring needed.
#[derive(Debug, Clone, Deserialize, SpecGrammar)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
#[grammar(group = "selection")]
pub enum SelectionRuleSpec {
    /// The leaf: every scored symbol eligible for either side (the full
    /// universe). The implicit `of:` default; rarely written explicitly.
    /// See [`crate::strategies::basket::Everything`].
    #[grammar(kind = "selection", output = "selection")]
    Everything,

    /// Take the `longs` highest-scoring symbols long, the `shorts`
    /// lowest-scoring short — ranked within [`of`](Self) (default the
    /// full universe). See [`crate::strategies::basket::top_bottom`].
    #[grammar(kind = "selection", output = "selection")]
    TopBottom {
        /// Number of top-ranked symbols to hold long.
        longs: usize,
        /// Number of bottom-ranked symbols to hold short.
        shorts: usize,
        /// Inner rule to rank within; defaults to the full universe (`!everything`).
        #[serde(default)]
        of: Box<SelectionRuleSpec>,
    },

    /// Long every symbol scoring at/above `long_min`; short at/below
    /// `short_max` — applied within [`of`](Self) (default the full
    /// universe). See [`crate::strategies::basket::threshold`].
    #[grammar(kind = "selection", output = "selection")]
    Threshold {
        /// Minimum score to hold a symbol long.
        long_min: Real,
        /// Maximum score to hold a symbol short.
        short_max: Real,
        /// Inner rule to rank within; defaults to the full universe (`!everything`).
        #[serde(default)]
        of: Box<SelectionRuleSpec>,
    },

    /// Long the top `long_q` fraction, short the bottom `short_q` — of
    /// [`of`](Self)'s per-side candidate sets (default the full universe).
    /// See [`crate::strategies::basket::quantile`].
    #[grammar(kind = "selection", output = "selection")]
    Quantile {
        /// Top score-quantile held long, a fraction in `[0, 1]`.
        long_q: Real,
        /// Bottom score-quantile held short, a fraction in `[0, 1]`.
        short_q: Real,
        /// Inner rule to rank within; defaults to the full universe (`!everything`).
        #[serde(default)]
        of: Box<SelectionRuleSpec>,
    },
}

impl Default for SelectionRuleSpec {
    /// The implicit `of:` inner: the [`Everything`] leaf.
    ///
    /// [`Everything`]: crate::strategies::basket::Everything
    fn default() -> Self {
        SelectionRuleSpec::Everything
    }
}

impl SelectionRuleSpec {
    /// Build the (possibly composed) [`Selection`] this spec describes.
    /// Each rule's `of:` inner defaults to [`Everything`], so a bare
    /// `!top_bottom { longs, shorts }` ranks the full universe while
    /// `!top_bottom { longs, shorts, of: !threshold { .. } }` ranks the
    /// threshold survivors.
    ///
    /// [`Everything`]: crate::strategies::basket::Everything
    fn build(&self) -> Box<dyn Selection<Symbol>> {
        match self {
            SelectionRuleSpec::Everything => Box::new(Everything),
            SelectionRuleSpec::TopBottom { longs, shorts, of } => {
                Box::new(TopBottom::of(DynSelection(of.build()), *longs, *shorts))
            }
            SelectionRuleSpec::Threshold {
                long_min,
                short_max,
                of,
            } => Box::new(Threshold::of(
                DynSelection(of.build()),
                *long_min,
                *short_max,
            )),
            SelectionRuleSpec::Quantile {
                long_q,
                short_q,
                of,
            } => Box::new(Quantile::of(DynSelection(of.build()), *long_q, *short_q)),
        }
    }
}

/// YAML surface for a declared basket [`Universe`](crate::strategies::basket::Universe).
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
#[derive(Debug, Clone, Deserialize, SpecGrammar)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
#[grammar(group = "universe")]
pub enum UniverseSpec {
    /// Strict declared universe: every listed symbol must be present on
    /// every bar (absence panics); readiness gates on all listed symbols
    /// scoring `Some`. Wraps [`crate::strategies::basket::AllOf`].
    #[grammar(kind = "universe", output = "none")]
    AllOf(Vec<String>),

    /// Lax declared universe: restrict to the listed subset but silently
    /// skip absent or still-unready members. Wraps
    /// [`crate::strategies::basket::AnyOf`].
    #[grammar(kind = "universe", output = "none")]
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
    /// `NodeSpec` tree with `!arg SYM` placeholders where the current
    /// symbol should be substituted.
    pub score: SpecTemplate<NodeSpec>,

    /// The per-symbol sizing source: the per-leg `ValueFraction`
    /// magnitude every selected symbol is entered at. Same shape as
    /// `score` — normal `NodeSpec` with `!arg SYM` placeholders.
    ///
    /// For the equal-weight common case (100% gross across an N-symbol
    /// basket), write `!equal_weight <n_legs>` — the constant `1.0 /
    /// n_legs` per leg. No `!arg` is needed there since equal-weight
    /// doesn't depend on the symbol.
    pub sizing: SpecTemplate<NodeSpec>,

    /// Declared symbol universe — `!all_of [...]` (strict: error on
    /// absence, wait until every listed symbol is ready) or `!any_of
    /// [...]` (lax: silently skip absent / unready). Omitted means the
    /// default floating universe (every symbol seen in the snapshot is
    /// picked up on first sight). See [`UniverseSpec`].
    #[serde(default)]
    pub universe: Option<UniverseSpec>,

    /// The **rebalance gate**: a boolean signal deciding, on each bar,
    /// whether the basket re-runs its selection and issues resize
    /// orders. Defaults to `!every 1` (fire every bar — preserves the
    /// pre-`rebalance_on` re-rank-every-bar behavior). Common
    /// non-default: `!every 5` for weekly (on a daily strategy),
    /// `!every 20` for ~monthly, `!is_weekday` composed with a
    /// calendar signal for weekday-only rebalances.
    ///
    /// A `None` reading (from a still-warming user signal) is treated as
    /// `false` — the safe default; the basket sits between rebalances
    /// rather than trading through unsettled data.
    #[serde(default)]
    pub rebalance_on: Option<NodeSpec>,

    /// **Balance the two sides' target sizes** at each rebalance, so that
    /// Σ long_sizes == Σ short_sizes. The smaller per-side sum is taken
    /// as the target gross-per-side (never levers up); a one-sided
    /// selection passes through unscaled, since there is no counter-side
    /// to balance against.
    ///
    /// **On by default** — an unbalanced basket carries net exposure its
    /// ranking never asked for. Set `false` to keep the raw per-leg sizes.
    #[serde(default = "default_balance_sides")]
    pub balance_sides: bool,

    /// Per-leg protective levels — same shape as the single-asset
    /// `long:` / `short:` spec sides but templated (`!arg SYM` for the
    /// current symbol). Each side's `stop_loss` and `take_profit` is an
    /// `NodeSpec` template built once per new symbol; `!entry` / `!peak`
    /// / `!trough` inside the template read against *that* symbol's
    /// [`Position`], letting fixed / ATR / trailing stops compose
    /// exactly as they do on `SingleAssetStrategy`.
    ///
    /// `enter` / `exit` on this side are ignored — basket uses its
    /// [`selection`](Self::selection) rule to decide per-symbol side.
    #[serde(default)]
    pub long: Option<BasketSideSpec>,
    #[serde(default)]
    pub short: Option<BasketSideSpec>,

    /// Free-form document metadata for external tooling. Parsed, carried, and
    /// never interpreted — see [`spec::meta`](crate::spec::meta).
    #[serde(default)]
    pub meta: Option<Meta>,
}

/// `balance_sides` defaults to `true` — see
/// [`BasketStrategySpec::balance_sides`]. A plain `#[serde(default)]`
/// would give `false`, which is the opt-out, not the default.
fn default_balance_sides() -> bool {
    true
}

/// Per-leg protective template for a [`BasketStrategySpec`] side. Only
/// the two protective fields are honored — `enter` / `exit` semantics
/// live on the basket's [`selection`](BasketStrategySpec::selection)
/// rule, which the ranking output supplies.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BasketSideSpec {
    #[serde(default)]
    pub stop_loss: Option<SpecTemplate<NodeSpec>>,
    #[serde(default)]
    pub take_profit: Option<SpecTemplate<NodeSpec>>,
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
    /// they always read `None` there. Inside the per-leg protective level
    /// templates ([`long`](Self::long) / [`short`](Self::short) with their
    /// `stop_loss:` / `take_profit:` fields) they *do* mean something —
    /// the factory receives that symbol's own `Position` so `!entry`
    /// reads the entry price of *that* leg. The shared `Book` anchor is
    /// wired everywhere, so book-anchored sizing recipes
    /// (`!drawdown_throttle`, `!equity_vol_target`, `!fractional_kelly`)
    /// work on the basket's aggregate equity curve.
    pub fn build(&self, initial_equity: Real, schema: &Arc<Schema>) -> DynBasketStrategy {
        self.try_build(initial_equity, schema)
            .unwrap_or_else(|e| panic!("{e}"))
    }

    /// The fallible twin of [`build`](Self::build).
    ///
    /// The eager parts (the rebalance gate) are built through `try_build`
    /// directly. The per-symbol `score` / `sizing` / protective-level templates
    /// can't be, because their factories run lazily inside the driver — so each
    /// is validated once here against a probe symbol (see `probe_template`),
    /// which is what makes those closures' remaining panic unreachable.
    pub fn try_build(
        &self,
        initial_equity: Real,
        schema: &Arc<Schema>,
    ) -> Result<DynBasketStrategy, String> {
        let strat = BasketStrategy::<Symbol>::with_initial_equity(initial_equity);
        let book = strat.book();

        // Probe every lazily-built template before wiring any of them.
        let probe_anchor = Position::new();
        probe_template(&self.score, "score", &probe_anchor, &book, schema)?;
        probe_template(&self.sizing, "sizing", &probe_anchor, &book, schema)?;
        for (side, slots) in [("long", &self.long), ("short", &self.short)] {
            let Some(side_spec) = slots else { continue };
            if let Some(t) = &side_spec.stop_loss {
                let slot = if side == "long" {
                    "long.stop_loss"
                } else {
                    "short.stop_loss"
                };
                probe_template(t, slot, &probe_anchor, &book, schema)?;
            }
            if let Some(t) = &side_spec.take_profit {
                let slot = if side == "long" {
                    "long.take_profit"
                } else {
                    "short.take_profit"
                };
                probe_template(t, slot, &probe_anchor, &book, schema)?;
            }
        }

        let score_template = self.score.clone();
        let book_score = book.clone();
        let schema_score = schema.clone();
        let strat = strat.scored_by(move |sym: &Symbol| {
            let concrete = build_per_symbol(&score_template, sym, "score");
            let anchor = Position::new();
            let dyn_ind: AnyChain =
                concrete.build(&anchor, &book_score, None, &schema_score, Root::blessed(&leg_root(sym)));
            dyn_ind.probed_real("score")
        });

        let sizing_template = self.sizing.clone();
        let book_sizing = book.clone();
        let schema_sizing = schema.clone();
        let strat = strat.sized_by(move |sym: &Symbol| {
            let concrete = build_per_symbol(&sizing_template, sym, "sizing");
            let anchor = Position::new();
            let dyn_ind: AnyChain =
                concrete.build(&anchor, &book_sizing, None, &schema_sizing, Root::blessed(&leg_root(sym)));
            dyn_ind.probed_real("sizing")
        });

        let strat = strat.selection(DynSelection(self.selection.build()));

        let strat = match &self.universe {
            Some(UniverseSpec::AllOf(syms)) => strat.all_of(syms.iter().map(crate::types::symbol)),
            Some(UniverseSpec::AnyOf(syms)) => strat.any_of(syms.iter().map(crate::types::symbol)),
            None => strat,
        };

        // Rebalance gate: default is `Every::new(1)` (every bar) so an
        // omitted `rebalance_on:` preserves the pre-refactor behavior. A
        // supplied signal is built against a dummy anchor / the shared
        // book — same convention as basket score/sizing templates.
        let strat = if let Some(rebalance_spec) = &self.rebalance_on {
            let anchor = Position::new();
            // `root: None` — the gate is basket-wide, not per-leg, so there is
            // no "this series" for it to mean. Cadence / calendar signals
            // (`!every`, `!monthly`) need no asset; one that reads a price
            // must name it with `!pick { symbol: ... }`.
            let dyn_ind: AnyChain =
                rebalance_spec.try_build(&anchor, &book, None, schema, Root::sole())?;
            strat.rebalance_on(dyn_ind.into_bool()?)
        } else {
            strat
        };

        // Per-leg protective levels — each side's stop_loss / take_profit
        // is a `SpecTemplate<NodeSpec>` built per-symbol and anchored
        // against *that* symbol's Position (not a dummy — this is where
        // `!entry` / `!peak` / `!trough` actually mean something in a
        // basket).
        let strat = if let Some(long) = &self.long {
            let mut strat = strat;
            if let Some(t) = long.stop_loss.clone() {
                let book_c = book.clone();
                let schema_c = schema.clone();
                strat = strat.long_stop_loss(move |sym: &Symbol, pos: &Position| {
                    let concrete = build_per_symbol(&t, sym, "long.stop_loss");
                    let dyn_ind: AnyChain =
                        concrete.build(pos, &book_c, None, &schema_c, Root::blessed(&leg_root(sym)));
                    dyn_ind.probed_real("long.stop_loss")
                });
            }
            if let Some(t) = long.take_profit.clone() {
                let book_c = book.clone();
                let schema_c = schema.clone();
                strat = strat.long_take_profit(move |sym: &Symbol, pos: &Position| {
                    let concrete = build_per_symbol(&t, sym, "long.take_profit");
                    let dyn_ind: AnyChain =
                        concrete.build(pos, &book_c, None, &schema_c, Root::blessed(&leg_root(sym)));
                    dyn_ind.probed_real("long.take_profit")
                });
            }
            strat
        } else {
            strat
        };
        let strat = if let Some(short) = &self.short {
            let mut strat = strat;
            if let Some(t) = short.stop_loss.clone() {
                let book_c = book.clone();
                let schema_c = schema.clone();
                strat = strat.short_stop_loss(move |sym: &Symbol, pos: &Position| {
                    let concrete = build_per_symbol(&t, sym, "short.stop_loss");
                    let dyn_ind: AnyChain =
                        concrete.build(pos, &book_c, None, &schema_c, Root::blessed(&leg_root(sym)));
                    dyn_ind.probed_real("short.stop_loss")
                });
            }
            if let Some(t) = short.take_profit.clone() {
                let book_c = book.clone();
                let schema_c = schema.clone();
                strat = strat.short_take_profit(move |sym: &Symbol, pos: &Position| {
                    let concrete = build_per_symbol(&t, sym, "short.take_profit");
                    let dyn_ind: AnyChain =
                        concrete.build(pos, &book_c, None, &schema_c, Root::blessed(&leg_root(sym)));
                    dyn_ind.probed_real("short.take_profit")
                });
            }
            strat
        } else {
            strat
        };

        let strat = strat.balance_sides(self.balance_sides);

        Ok(DynBasketStrategy { inner: strat })
    }
}

/// The blessed series for one leg's chain: every `source:`-omitted leaf in a
/// score / sizing / protective template reads *that leg's* symbol out of the
/// snapshot.
///
/// This is what makes `!arg SYM` **optional** rather than required in a basket
/// template — `score: !rsi { period: 14 }` and the fully-spelled
/// `score: !rsi { period: 14, source: !close { source: !pick { symbol: !arg SYM } } }`
/// now build the same chain. The explicit form keeps working (it resolves
/// through [`build_per_symbol`] exactly as before), and stays the way to read
/// a *different* symbol per leg — a hedge ratio against a common benchmark,
/// say — which the implicit root can't express.
fn leg_root(sym: &str) -> Selector<Symbol> {
    Selector::by_symbol(crate::types::symbol(sym))
}

/// Resolve a per-symbol template into a concrete `NodeSpec` by supplying
/// `SYM` from `sym`. Panics with a descriptive message on failure — the
/// build-time template resolution is a config error, not a runtime
/// condition to recover from, so a loud panic surfaces the bad YAML.
fn build_per_symbol(
    template: &SpecTemplate<NodeSpec>,
    sym: &str,
    slot: &'static str,
) -> NodeSpec {
    try_build_per_symbol(template, sym, slot).unwrap_or_else(|e| panic!("{e}"))
}

/// The fallible twin of [`build_per_symbol`].
fn try_build_per_symbol(
    template: &SpecTemplate<NodeSpec>,
    sym: &str,
    slot: &'static str,
) -> Result<NodeSpec, String> {
    let mut args = HashMap::new();
    args.insert("SYM".to_string(), Value::String(sym.to_string()));
    template
        .build(&args)
        .map_err(|e| format!("basket {slot} template build failed for symbol {sym:?}: {e}"))
}

/// The stand-in symbol the build-time probe substitutes for `!arg SYM`.
///
/// Deliberately not a plausible ticker: it never matches a real snapshot entry,
/// so a probe chain is inert even if one were accidentally retained.
const PROBE_SYMBOL: &str = "__fugazi_probe__";

/// Validate a per-symbol template by building it once, at spec-build time,
/// against [`PROBE_SYMBOL`].
///
/// The per-symbol factories build their chain on **first sight of a symbol**,
/// inside `BasketStrategy::update` — a context with no error path to return
/// through, which is why those closures still `panic!`. The only thing that
/// varies between symbols is the `!arg SYM` substitution and the blessed root
/// selector, and neither can change *which* tags the tree contains or what
/// types they produce. So a template that builds for one symbol builds for
/// every symbol, and checking once here is enough to turn the factories'
/// remaining panic into a proven-unreachable invariant — while moving the
/// diagnostic to load time, where the author can act on it.
fn probe_template(
    template: &SpecTemplate<NodeSpec>,
    slot: &'static str,
    anchor: &Position,
    book: &Book,
    schema: &Arc<Schema>,
) -> Result<(), String> {
    let concrete = try_build_per_symbol(template, PROBE_SYMBOL, slot)?;
    concrete
        .try_build(anchor, book, None, schema, Root::blessed(&leg_root(PROBE_SYMBOL)))
        .map(|_| ())
}

// ---------------------------------------------------------------------------
// DynBasketStrategy: CLI-owned wrapper around BasketStrategy<Symbol>
// ---------------------------------------------------------------------------

/// The CLI's built-basket handle. Wraps a
/// [`BasketStrategy<Symbol>`](crate::strategies::BasketStrategy) whose
/// per-symbol score / sizing factories were assembled from
/// [`SpecTemplate<NodeSpec>`](SpecTemplate).
///
/// Implements [`Strategy`] by delegation, so it drops
/// into [`crate::backtest::run`] unchanged (once the CLI dispatch grows
/// a `basket:` prefix — a follow-up).
pub struct DynBasketStrategy {
    inner: BasketStrategy<Symbol>,
}

impl Strategy for DynBasketStrategy {
    type Input = Snapshot<Symbol>;
    type Symbol = Symbol;

    fn update(&mut self, input: Snapshot<Symbol>) {
        self.inner.update(input);
    }

    fn trade(&self, wallet: &mut dyn Wallet<Symbol>) {
        self.inner.trade(wallet);
    }

    fn on_fill(&mut self, order: &Order<Symbol>) {
        self.inner.on_fill(order);
    }

    fn is_ready(&self) -> bool {
        self.inner.is_ready()
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

impl DynBasketStrategy {
    /// A clone of the shared [`Book`] anchor — for downstream book-side
    /// diagnostics and (once CLI dispatch grows a basket path) initial
    /// equity assertions.
    pub fn book(&self) -> Book<Symbol> {
        self.inner.book()
    }

    /// Serialize the wrapped basket's runtime state for run resuming.
    pub fn save_state(&self) -> serde_json::Value {
        self.inner.save_state()
    }

    /// Restore state produced by [`save_state`](Self::save_state).
    pub fn restore_state(&mut self, state: &serde_json::Value) -> Result<(), String> {
        self.inner.restore_state(state)
    }

    /// Grid-wide readiness across the currently-built per-symbol score /
    /// sizing chains and the rebalance gate — pass-through to
    /// [`BasketStrategy::stable_bars`](crate::strategies::BasketStrategy::stable_bars).
    ///
    /// **Lazy readiness contract**: a basket's per-symbol chains are
    /// built on first sight, so a freshly-built strategy reports the
    /// rebalance-signal period only. Feed one representative snapshot
    /// via [`update`](Strategy::update) before probing so the per-symbol
    /// chains exist. See the underlying method for details.
    pub fn stable_bars(&self) -> usize {
        self.inner.stable_bars()
    }

    /// Warm-up-only readiness (ignoring IIR settling) — pass-through to
    /// [`BasketStrategy::warm_up_bars`](crate::strategies::BasketStrategy::warm_up_bars).
    /// Used by `optimize --walkforward --keep-unstable`.
    ///
    /// Same lazy-readiness caveat as [`stable_bars`](Self::stable_bars).
    pub fn warm_up_bars(&self) -> usize {
        self.inner.warm_up_bars()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PaperWallet;
    use crate::types::{Atom, Selector};

    fn candle(price: Real) -> Candle {
        Candle::new(price, price, price, price, 0.0)
    }

    fn snap_of(entries: &[(&'static str, Real)]) -> Snapshot<Symbol> {
        let mut s = Snapshot::new();
        for &(sym, close) in entries {
            let atom = Atom::new(candle(close));
            s.push(Some(crate::types::symbol(sym)), None, atom);
        }
        s
    }

    fn schema() -> Arc<Schema> {
        Schema::empty()
    }

    /// The point of the build-time probe: a per-symbol template that can't
    /// build is caught *here*, not on the first bar that mentions a symbol.
    ///
    /// Without the probe, `score` is only constructed inside
    /// `BasketStrategy::update` — deep in the driver, with no error path — so a
    /// bad `!get` in it would abort a run that had already started.
    #[test]
    fn a_bad_per_symbol_template_is_rejected_at_build_not_mid_run() {
        let yaml = r#"
            selection: !top_bottom { longs: 1, shorts: 1 }
            score: !get { key: no_such_column }
            sizing: !value 1.0
        "#;
        let spec = BasketStrategySpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        let err = spec
            .try_build(1.0, &schema())
            .err()
            .expect("the probe must reject this");
        assert!(err.contains("no_such_column"), "{err}");
        assert!(err.contains("!get"), "carries the tag trail: {err}");
    }

    /// A typo inside a deferred template is a *load* error, like a typo in any
    /// eagerly-parsed slot.
    ///
    /// The template's shape doesn't depend on which symbol the driver binds, so
    /// `SpecTemplate`'s `Deserialize` typed-parses a probe copy with `!arg SYM`
    /// held as a hole. Before that, `score:` was captured as an untyped tree and
    /// a misspelled tag survived the load, `fugazi check`, and everything else
    /// until the first bar that instantiated it.
    #[test]
    fn a_misspelled_tag_inside_a_template_fails_the_load() {
        let yaml = r#"
            selection: !top_bottom { longs: 1, shorts: 1 }
            score: !smaa { source: !close { source: !pick { symbol: !arg SYM } }, period: 20 }
            sizing: !value 1.0
        "#;
        let err = BasketStrategySpec::from_text_with_params(yaml, &HashMap::new())
            .expect_err("a misspelled tag must not load");
        let err = format!("{err:#}");
        assert!(err.contains("smaa"), "{err}");
    }

    /// The same eagerness, one level in: a well-spelled tag with a field that
    /// isn't its own.
    #[test]
    fn an_unknown_field_inside_a_template_fails_the_load() {
        let yaml = r#"
            selection: !top_bottom { longs: 1, shorts: 1 }
            score: !sma { source: !close { source: !pick { symbol: !arg SYM } }, perid: 20 }
            sizing: !value 1.0
        "#;
        let err = BasketStrategySpec::from_text_with_params(yaml, &HashMap::new())
            .expect_err("a misspelled field must not load");
        let err = format!("{err:#}");
        assert!(err.contains("perid"), "{err}");
    }

    /// …and the eager parse must not reject what the driver will happily build:
    /// `!arg SYM` stands in a `symbol:` (string) position here, and the probe
    /// answers it with a string rather than failing the typed parse.
    #[test]
    fn a_templated_arg_still_loads() {
        let yaml = r#"
            selection: !top_bottom { longs: 1, shorts: 1 }
            score: !sma { source: !close { source: !pick { symbol: !arg SYM } }, period: 20 }
            sizing: !value 1.0
        "#;
        BasketStrategySpec::from_text_with_params(yaml, &HashMap::new())
            .expect("a well-formed template loads");
    }

    #[test]
    fn deserializes_a_full_basket_spec() {
        let yaml = r#"
            selection: !top_bottom { longs: 2, shorts: 2 }
            score:
              !roc
                source: !close { source: !pick { symbol: !arg SYM } }
                period: 5
            sizing: !equal_weight 4
        "#;
        let spec = BasketStrategySpec::from_text_with_params(
            yaml,
            &HashMap::new(),
        )
        .unwrap();
        match spec.selection {
            SelectionRuleSpec::TopBottom { longs, shorts, of } => {
                assert_eq!(longs, 2);
                assert_eq!(shorts, 2);
                // No `of:` supplied → defaults to the Everything leaf.
                assert!(matches!(*of, SelectionRuleSpec::Everything));
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
    fn composed_selection_parses_nested_of() {
        let yaml = "!top_bottom { longs: 2, shorts: 2, of: !threshold { long_min: 0.5, short_max: -0.5 } }";
        let rule: SelectionRuleSpec = serde_norway::from_str(yaml).unwrap();
        match rule {
            SelectionRuleSpec::TopBottom { longs, shorts, of } => {
                assert_eq!((longs, shorts), (2, 2));
                match *of {
                    SelectionRuleSpec::Threshold {
                        long_min,
                        short_max,
                        of,
                    } => {
                        assert_eq!(long_min, 0.5);
                        assert_eq!(short_max, -0.5);
                        // Inner rule's own `of:` defaults to Everything.
                        assert!(matches!(*of, SelectionRuleSpec::Everything));
                    }
                    other => panic!("inner should be Threshold, got {other:?}"),
                }
            }
            other => panic!("outer should be TopBottom, got {other:?}"),
        }
    }

    #[test]
    fn composed_selection_gates_ranked_picks_through_threshold() {
        // top_bottom(2,2) OF threshold(85, 15): the threshold admits {A,B}
        // long (>= 85) and {D} short (<= 15); C (80) sits in the gap. The
        // ranked top-2 / bottom-2 therefore draw only from the survivors,
        // so C ends flat — where a bare top_bottom(2,2) would have shorted
        // it. Proves the `of:` inner actually narrows the pool.
        let yaml = r#"
            selection: !top_bottom { longs: 2, shorts: 2, of: !threshold { long_min: 85.0, short_max: 15.0 } }
            score: !close { source: !pick { symbol: !arg SYM } }
            sizing: !value 0.2
        "#;
        let spec =
            BasketStrategySpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        let mut strat = spec.build(10_000.0, &schema());
        let mut wallet: PaperWallet<Symbol> = PaperWallet::new(10_000.0);

        for _ in 0..2 {
            for (sym, px) in [("A", 100.0), ("B", 90.0), ("C", 80.0), ("D", 10.0)] {
                for fill in wallet.update(crate::types::symbol(sym), candle(px)) {
                    strat.on_fill(&fill);
                }
            }
            strat.update(snap_of(&[
                ("A", 100.0),
                ("B", 90.0),
                ("C", 80.0),
                ("D", 10.0),
            ]));
            strat.trade(&mut wallet);
        }
        assert!(wallet.position(&crate::types::symbol("A")).amount > 0.0, "A long");
        assert!(wallet.position(&crate::types::symbol("B")).amount > 0.0, "B long");
        assert!(
            wallet.position(&crate::types::symbol("C")).amount.abs() < 1e-9,
            "C gated out by threshold → flat"
        );
        assert!(wallet.position(&crate::types::symbol("D")).amount < 0.0, "D short");
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
        let mut wallet: PaperWallet<Symbol> = PaperWallet::new(10_000.0);

        for _ in 0..2 {
            let bar_a = candle(100.0);
            let bar_b = candle(50.0);
            let bar_c = candle(25.0);
            for fill in wallet.update(crate::types::symbol("A"), bar_a) {
                strat.on_fill(&fill);
            }
            for fill in wallet.update(crate::types::symbol("B"), bar_b) {
                strat.on_fill(&fill);
            }
            for fill in wallet.update(crate::types::symbol("C"), bar_c) {
                strat.on_fill(&fill);
            }
            strat.update(snap_of(&[("A", 100.0), ("B", 50.0), ("C", 25.0)]));
            strat.trade(&mut wallet);
        }
        assert!(
            wallet.position(&crate::types::symbol("A")).amount > 0.0,
            "A should be long"
        );
        assert!(
            wallet.position(&crate::types::symbol("C")).amount < 0.0,
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
        let mut wallet: PaperWallet<Symbol> = PaperWallet::new(10_000.0);
        for _ in 0..2 {
            for fill in wallet.update(crate::types::symbol("X"), candle(200.0)) {
                strat.on_fill(&fill);
            }
            for fill in wallet.update(crate::types::symbol("Y"), candle(100.0)) {
                strat.on_fill(&fill);
            }
            strat.update(snap_of(&[("X", 200.0), ("Y", 100.0)]));
            strat.trade(&mut wallet);
        }
        assert!(wallet.position(&crate::types::symbol("X")).amount > 0.0);
        assert!(wallet.position(&crate::types::symbol("Y")).amount.abs() < 1e-9);
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
        let mut wallet: PaperWallet<Symbol> = PaperWallet::new(10_000.0);

        for _ in 0..2 {
            for fill in wallet.update(crate::types::symbol("X"), candle(200.0)) {
                strat.on_fill(&fill);
            }
            for fill in wallet.update(crate::types::symbol("Y"), candle(100.0)) {
                strat.on_fill(&fill);
            }
            for fill in wallet.update(crate::types::symbol("Z"), candle(500.0)) {
                strat.on_fill(&fill);
            }
            strat.update(snap_of(&[("X", 200.0), ("Y", 100.0), ("Z", 500.0)]));
            strat.trade(&mut wallet);
        }
        assert!(wallet.position(&crate::types::symbol("X")).amount > 0.0, "X long");
        assert!(wallet.position(&crate::types::symbol("Y")).amount < 0.0, "Y short");
        assert!(
            wallet.position(&crate::types::symbol("Z")).amount.abs() < 1e-9,
            "Z is outside the declared universe: no trade"
        );
    }

    #[test]
    #[should_panic(expected = "strict universe requires")]
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
    fn rebalance_on_defaults_to_none_and_omitting_matches_current_behavior() {
        // Sanity: omitting `rebalance_on:` parses cleanly. The default
        // gate is installed at build time (`Every::new(1)`), so an
        // omitted YAML field behaves identically to the pre-refactor
        // basket.
        let yaml = r#"
            selection: !top_bottom { longs: 1, shorts: 1 }
            score: !close { source: !pick { symbol: !arg SYM } }
            sizing: !value 0.5
        "#;
        let spec =
            BasketStrategySpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        assert!(spec.rebalance_on.is_none());
    }

    #[test]
    fn rebalance_on_every_5_only_re_ranks_periodically() {
        // Rebalance every 5 bars — no orders on bars 1..4 (gate is
        // false), a queued order on bar 5, fill on bar 6.
        let yaml = r#"
            selection: !top_bottom { longs: 1, shorts: 1 }
            score: !close { source: !pick { symbol: !arg SYM } }
            sizing: !value 0.5
            rebalance_on: !every 5
        "#;
        let spec =
            BasketStrategySpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        let mut strat = spec.build(10_000.0, &schema());
        let mut wallet: PaperWallet<Symbol> = PaperWallet::new(10_000.0);
        for _ in 0..4 {
            for fill in wallet.update(crate::types::symbol("A"), candle(100.0)) {
                strat.on_fill(&fill);
            }
            for fill in wallet.update(crate::types::symbol("B"), candle(50.0)) {
                strat.on_fill(&fill);
            }
            strat.update(snap_of(&[("A", 100.0), ("B", 50.0)]));
            strat.trade(&mut wallet);
        }
        assert!(wallet.orders().is_empty(), "no orders in the first 4 off-cycle bars");
        // Bar 5: gate fires. Bar 6: order fills.
        for _ in 0..2 {
            for fill in wallet.update(crate::types::symbol("A"), candle(100.0)) {
                strat.on_fill(&fill);
            }
            for fill in wallet.update(crate::types::symbol("B"), candle(50.0)) {
                strat.on_fill(&fill);
            }
            strat.update(snap_of(&[("A", 100.0), ("B", 50.0)]));
            strat.trade(&mut wallet);
        }
        assert!(
            wallet.position(&crate::types::symbol("A")).amount > 0.0,
            "A long after the first rebalance fires"
        );
    }

    #[test]
    fn rebalance_on_never_freezes_the_basket() {
        // `!never` is a ValueBool::false — the basket never rebalances.
        let yaml = r#"
            selection: !top_bottom { longs: 1, shorts: 1 }
            score: !close { source: !pick { symbol: !arg SYM } }
            sizing: !value 0.5
            rebalance_on: !never
        "#;
        let spec =
            BasketStrategySpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        let mut strat = spec.build(10_000.0, &schema());
        let mut wallet: PaperWallet<Symbol> = PaperWallet::new(10_000.0);
        for _ in 0..8 {
            for fill in wallet.update(crate::types::symbol("A"), candle(100.0)) {
                strat.on_fill(&fill);
            }
            for fill in wallet.update(crate::types::symbol("B"), candle(50.0)) {
                strat.on_fill(&fill);
            }
            strat.update(snap_of(&[("A", 100.0), ("B", 50.0)]));
            strat.trade(&mut wallet);
        }
        assert!(wallet.orders().is_empty(), "!never must not trade");
    }

    #[test]
    fn vol_target_with_per_symbol_source_survives_multi_symbol_snapshot() {
        // `!vol_target { source: !pick { symbol: !arg SYM }, ... }` — each
        // leg's sizing chain projects its own asset, so the sole-atom panic
        // that the sourceless shortcut would fire on a multi-entry snapshot
        // never fires here. Just proves the build path doesn't blow up.
        let yaml = r#"
            selection: !top_bottom { longs: 1, shorts: 1 }
            score: !close { source: !pick { symbol: !arg SYM } }
            sizing:
              !vol_target
                source: !pick { symbol: !arg SYM }
                target: 0.20
                window: 3
                bars_per_year: 252
        "#;
        let spec =
            BasketStrategySpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        let mut strat = spec.build(10_000.0, &schema());
        let mut wallet: PaperWallet<Symbol> = PaperWallet::new(10_000.0);
        // Drive a few bars over two symbols with varying prices so the
        // sizing chain settles and the top/bottom selection alternates.
        for i in 0..8 {
            let a = 100.0 + (i as Real);
            let b = 50.0 - (i as Real);
            for fill in wallet.update(crate::types::symbol("A"), candle(a)) {
                strat.on_fill(&fill);
            }
            for fill in wallet.update(crate::types::symbol("B"), candle(b)) {
                strat.on_fill(&fill);
            }
            strat.update(snap_of(&[("A", a), ("B", b)]));
            strat.trade(&mut wallet);
        }
    }

    #[test]
    fn atr_risk_with_per_symbol_source_survives_multi_symbol_snapshot() {
        // Twin of the vol_target case for the ATR-risk sizing recipe.
        let yaml = r#"
            selection: !top_bottom { longs: 1, shorts: 1 }
            score: !close { source: !pick { symbol: !arg SYM } }
            sizing:
              !atr_risk
                source: !pick { symbol: !arg SYM }
                risk_frac: 0.01
                period: 3
                atr_multiple: 2.0
        "#;
        let spec =
            BasketStrategySpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        let mut strat = spec.build(10_000.0, &schema());
        let mut wallet: PaperWallet<Symbol> = PaperWallet::new(10_000.0);
        for i in 0..8 {
            let a = 100.0 + (i as Real);
            let b = 50.0 - (i as Real);
            for fill in wallet.update(crate::types::symbol("A"), candle(a)) {
                strat.on_fill(&fill);
            }
            for fill in wallet.update(crate::types::symbol("B"), candle(b)) {
                strat.on_fill(&fill);
            }
            strat.update(snap_of(&[("A", a), ("B", b)]));
            strat.trade(&mut wallet);
        }
    }

    #[test]
    fn balance_sides_defaults_on_and_parses_the_opt_out() {
        // Omitted → `true`. A plain `#[serde(default)]` would give `false`
        // here, which is the opt-out rather than the default.
        let base = r#"
            selection: !top_bottom { longs: 1, shorts: 1 }
            score: !close { source: !pick { symbol: !arg SYM } }
            sizing: !value 0.5
        "#;
        let spec = BasketStrategySpec::from_text_with_params(base, &HashMap::new()).unwrap();
        assert!(spec.balance_sides, "balance_sides defaults to true");

        let opted_out = format!("{base}    balance_sides: false\n");
        let spec =
            BasketStrategySpec::from_text_with_params(&opted_out, &HashMap::new()).unwrap();
        assert!(!spec.balance_sides);

        // The old spelling is now an unknown field, not a silently-ignored
        // one — `deny_unknown_fields` turns a stale document into an error.
        let stale = format!("{base}    dollar_neutral: true\n");
        assert!(BasketStrategySpec::from_text_with_params(&stale, &HashMap::new()).is_err());
    }

    #[test]
    fn parses_per_leg_protective_templates() {
        // Basket with per-leg protective levels — 5% stop-loss below entry.
        // The template uses `!entry` (a Position accessor) which is only
        // meaningful in the per-leg factory context; here we just verify
        // the spec parses and builds without panicking.
        let yaml = r#"
            selection: !top_bottom { longs: 1, shorts: 1 }
            score: !close { source: !pick { symbol: !arg SYM } }
            sizing: !value 0.5
            long:
              stop_loss: !mul { lhs: !entry, rhs: !value 0.95 }
            short:
              stop_loss: !mul { lhs: !entry, rhs: !value 1.05 }
        "#;
        let spec = BasketStrategySpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        assert!(spec.long.is_some());
        assert!(spec.short.is_some());
        // Build shouldn't panic.
        let _built = spec.build(1_000.0, &Schema::empty());
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
                period: !param FAST
            sizing: !value 0.5
        "#;
        let mut params = HashMap::new();
        params.insert("FAST".to_string(), Value::Number(10.into()));
        let spec = BasketStrategySpec::from_text_with_params(yaml, &params).unwrap();
        // The stored tree should carry `period: 10` (resolved) and
        // `symbol: {arg: "SYM"}` (deferred).
        let tree = spec.score.tree();
        let period = tree.pointer("/roc/period").unwrap();
        assert_eq!(period, &Value::Number(10.into()));
        let sym = tree.pointer("/roc/source/close/source/pick/symbol").unwrap();
        assert_eq!(sym, &serde_json::json!({"arg": "SYM"}));
    }
}
