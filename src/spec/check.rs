//! Shape-only validation of a strategy document — the parse behind both
//! `fugazi check strategy` and Python's `ta.check_spec`.
//!
//! `check` answers one question: *is this document well-formed?* Not *will it
//! run* — that needs data, an overlay schema, and values for every `!param` the
//! author left to the caller. Those are precisely what an authoring tool does
//! not have: a strategy is written once and parameterised per run, so at the
//! moment it is saved `FAST` and `SYMBOL` have no values and validating with
//! `params = {}` would refuse the document for being exactly what it is.
//!
//! So this pass substitutes hole-aware ([`params::substitute_for_check`]): an
//! unset required `!param` becomes a *hole* rather than an error, and the typed
//! parse fills each hole with a value of whatever type the field demands (see
//! [`undefined`]). Everything else is validated normally — an unknown tag, a
//! misspelled field, a slot handed the wrong type, a `!param` two positions
//! disagree about — and when nothing is left undetermined the document is
//! *built* too, which catches the one class of error a typed parse structurally
//! cannot (see [`CheckedSpec::built`]).
//!
//! What comes back is a [`CheckedSpec`]: the parsed spec, and — the half worth
//! as much as the verdict — one [`HoleTypes`] per placeholder, saying what type
//! each unset one has to be. That is answered *from the parse*, by the slots the
//! placeholder actually sits in, so a tool that has to type a parameter it has
//! never seen a value for does not have to guess from its name.
//!
//! ## The spec that comes back is not runnable
//!
//! A hole answers its field with a typed zero — `1` for an integer, `""` for a
//! string. So a document with unset placeholders parses into a spec whose
//! `period` is 1 and whose `symbol` is empty, and *driving* it would silently
//! backtest a strategy nobody wrote. [`CheckedSpec`] therefore hands back the
//! spec for inspection (its shape, its universe, its `meta:`) and both callers
//! stop there; the Python binding does not expose it at all. A document you
//! intend to run goes through [`load_document`](super::load_document) with real
//! values, which is the path that refuses a placeholder it cannot resolve.
//!
//! [`params::substitute_for_check`]: super::params::substitute_for_check

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value as Json;

use super::input::StrategyKind;
use super::runnable::StrategySpec;
use super::undefined::{self, HoleTypes, RequiredType, UndefinedOrigin};
use crate::market::{Real, Schema};

/// Cash a `check` build is seeded with. `check` never drives the strategy, so
/// the figure only has to be positive and unremarkable — nothing reads it.
pub const CHECK_CASH: Real = 100_000.0;

/// The verdict of a [`check_value`] pass: a document that parsed, plus
/// everything the pass learned about it on the way through.
#[derive(Debug)]
pub struct CheckedSpec {
    /// The parsed spec, with every unset placeholder standing as a typed zero.
    ///
    /// **Not runnable** — see the module docs. Safe to *ask* things of
    /// (`kind()`, `universe()`, `meta()`); driving it backtests the zeros.
    pub spec: StrategySpec,
    /// One entry per placeholder the document left unresolved, naming the
    /// type(s) its positions demand and the `type:` it declared, if any.
    /// Sorted by `(origin, name)`; empty when the document is fully determined.
    pub holes: Vec<HoleTypes>,
    /// Symbols the document reads through an explicit `!pick` — including the
    /// ones it also trades. Same walk, and same meaning, as the `reads` on a
    /// loaded spec: `check` has no data so it cannot say these are *present*,
    /// only that they are *required*.
    pub reads: Vec<String>,
    /// Whether the document was built as well as parsed.
    ///
    /// The build is the one check the typed parse structurally cannot do — a
    /// leaf with no asset to read in a shape that holds more than one — so it
    /// runs whenever the document is fully determined. It is skipped, and this
    /// reads `false`, in the four cases where building would report a *document*
    /// error for a document whose only gap is an input nobody supplied: an
    /// author-written `!undefined`, a placeholder standing in for a whole
    /// expression, a `!get` (whose build consults an overlay schema only real
    /// data can supply), and a single-asset `root:` left to the input — either
    /// the sole-atom selector or an unresolved placeholder.
    pub built: bool,
}

/// Hole-aware parse (and, when the document is fully determined, build) of an
/// already-`!import`-spliced untyped tree.
///
/// `kind` decides the shape — a `root:`-less document is structurally
/// indistinguishable from a `multi:` one — and is also what
/// [`root::apply_default`](super::root::apply_default) needs, which this applies
/// itself so the tree `check` validates is the one `run` would build.
///
/// The `Err` is an ordinary bad document: a parse failure, a placeholder two
/// positions contradict each other about, or a build error. Callers add their
/// own "loading `<label>`" context.
pub fn check_value(
    value: Json,
    kind: StrategyKind,
    params: &HashMap<String, Json>,
) -> anyhow::Result<CheckedSpec> {
    // Drained before anything writes to it, so a check is hermetic even if some
    // earlier one on this thread unwound past its own drain. It has to be here
    // rather than around the parse: `substitute_for_check` below is what logs a
    // placeholder's declared `type:`, so a drain after it would throw the
    // declarations away and report every placeholder as merely inferred.
    let _ = undefined::take_observations();
    // The defaulted keys land while the placeholders they carry are still
    // placeholders, so they resolve — or become holes — like any the author
    // wrote. Today that is the single-asset `root:`; the policy is
    // `root::apply_default`'s, this only says which shape.
    let value = super::root::apply_default(value, kind);
    // The site count is discarded: a report counts distinct placeholder
    // *names*, which is what a caller has to supply values for.
    let (value, _n_hole_sites) = super::params::substitute_for_check(value, params)?;

    // `!get` is the one leaf whose *build* consults the overlay schema, and
    // `check` has no data to derive one from. Note it while the tree is in hand.
    let reads_overlay = mentions_tag(&value, "get");
    // Into a `Vec` from the walk's `BTreeSet`: sorted and deduplicated already,
    // and a sequence is what both surfaces hand on.
    let reads: Vec<String> = super::reads::picked_symbols(&value).into_iter().collect();

    // The guard spans the *build* as well as the parse, and has to: a deferred
    // template body — a basket's `score:`, a portfolio's `weights:` — is not
    // reached by the typed parse at all. It is held as a raw tree and re-parsed
    // per symbol at build time, so a hole inside one arrives at that parse as
    // the bare sentinel mapping, and every scalar slot it sits in fails with an
    // `invalid type: map`. Dropping the guard after the parse makes every
    // basket or portfolio with a placeholder in a template body unloadable.
    //
    // It drops at the end of this function, which is before returning — a
    // check-mode guard escaping into a caller would make its *next* ordinary
    // load hole-aware, which is the one thing the flag must never do.
    let _guard = undefined::check_mode();
    let parsed = parse_shape(value, kind);
    // Drained here, between the parse and the build, on both the `Ok` and the
    // `Err` path: this is the ledger the report is made of, and leaving it for
    // the caller's error handling to forget would hand the *next* check on this
    // thread the failed document's placeholders as its own.
    let holes = undefined::take_observations();
    let spec = parsed?;
    reject_contradictory(&holes).map_err(anyhow::Error::msg)?;

    let built = is_determined(&spec, &holes, reads_overlay);
    if built {
        let schema = Arc::new(Schema::default());
        spec.try_build(CHECK_CASH, &schema, None)
            .map_err(super::backtest::build_error)?;
        // The build re-parses every template body, re-recording demands the
        // parse above already reported. Duplicates of what is in `holes`, so
        // they are dropped rather than merged — but dropped explicitly, because
        // the ledger is thread-local and nobody else drains it.
        let _ = undefined::take_observations();
    }
    Ok(CheckedSpec {
        spec,
        holes,
        reads,
        built,
    })
}

/// Route an untyped tree to its shape's typed parse. Hole-aware only because
/// the caller holds the [`check_mode`](undefined::check_mode) guard around it —
/// this function does not, so that the same guard can also cover the build.
///
/// A sixth document shape gets an arm here and nowhere else in `check`.
fn parse_shape(value: Json, kind: StrategyKind) -> anyhow::Result<StrategySpec> {
    macro_rules! parse {
        ($variant:ident, $ty:ty) => {
            undefined::from_json_value::<$ty>(value)
                .map(|s| StrategySpec::$variant(Box::new(s)))
                .map_err(anyhow::Error::new)
        };
    }
    match kind {
        StrategyKind::Single => parse!(Single, super::preset::StrategyRef),
        StrategyKind::Pairs => parse!(Pairs, super::pairs::PairsStrategySpec),
        StrategyKind::Basket => parse!(Basket, super::basket::BasketStrategySpec),
        StrategyKind::Multi => parse!(Multi, super::multi_asset::MultiAssetStrategySpec),
        StrategyKind::Portfolio => parse!(Portfolio, super::portfolio::PortfolioSpec),
    }
}

/// Is every input the build needs already in the document?
///
/// See [`CheckedSpec::built`] for what each `false` means. The rule is one-way
/// on purpose: skipping a build that would have succeeded costs a check nobody
/// missed, running one that cannot succeed reports a broken document to someone
/// whose document is fine.
fn is_determined(spec: &StrategySpec, holes: &[HoleTypes], reads_overlay: bool) -> bool {
    // An author-written `!undefined` is a gap the author declared; there is no
    // value to build from and none was ever going to be passed.
    let undefined_holes = holes.iter().any(|h| h.origin == UndefinedOrigin::Undefined);
    // A placeholder standing in for a whole *expression* has no node to stand
    // in for — picking one would either invent a type the value has not chosen
    // yet, or fail the build on a document whose only gap is a param value.
    let expr_holes = holes.iter().any(|h| h.used.contains(&RequiredType::Expr));
    // A single-asset root left to the input: `try_build` demands a symbol to
    // route orders through, and a single-series frame is what supplies it — so
    // building here would report a document the run will accept. Same for a root
    // still holding an unset placeholder: every other hole answers its field
    // with a typed zero the build proceeds on, a root cannot.
    let root_from_data = match spec {
        StrategySpec::Single(s) => is_sole_atom_root(s.root()) || s.root().has_hole(),
        _ => false,
    };
    !undefined_holes && !expr_holes && !reads_overlay && !root_from_data
}

/// Is this root the sole-atom selector — `!pick {}`, naming no symbol?
///
/// The one root `check` treats as *pending* rather than broken, because a
/// single-series input resolves it. Deliberately `as_pick`-shaped: a root that
/// is a nested expression, or an unresolved placeholder, also names no symbol,
/// and neither of those is something the input will fill in.
pub fn is_sole_atom_root(root: &super::root::RootSpec) -> bool {
    matches!(root.as_pick(), Some((None, _)))
}

/// Reject a placeholder required to be two different types — in two places, or
/// in one place that disagrees with the `type:` it declared.
///
/// This is decidable without any data and is always a real defect: no single
/// value can satisfy both positions, so the document can never run whatever the
/// caller supplies. Catching it is the whole point of *inferring* hole types
/// rather than just counting them — and of letting an author declare one, which
/// turns the same check into a statement about a single position rather than a
/// tie between two.
pub fn reject_contradictory(observations: &[HoleTypes]) -> Result<(), String> {
    let bad: Vec<String> = observations
        .iter()
        // Only named placeholders can contradict: an `!undefined` is keyed by
        // its own document path, so it is one position and cannot be two types.
        .filter(|hole| hole.origin == UndefinedOrigin::Param)
        .filter_map(|hole| {
            // `Expr` is not a *demand*: it says the placeholder stands where a
            // whole expression goes, and every scalar a caller can pass is one.
            // So it agrees with any other observation of the same name — `FAST`
            // used as a period and as `rhs:` is one number, not a contradiction
            // — and only the typed observations are counted here.
            let demanded = hole.demanded();
            let name = &hole.name;
            // A declared `type:` is a claim about the value, so a position
            // demanding something else is the same defect as two positions
            // demanding different things — caught one step earlier, and
            // attributable to the declaration rather than to a tie between two
            // slots.
            // `compatible_with`, not `!=`: a refined slot type and the
            // coarser declaration it refines are one claim, not two. A
            // `!pick { symbol: !param { key: SYM, type: string } }` demands
            // `Symbol` at the slot and declares `Str`, and has always been
            // valid — the refined types must not make it a contradiction.
            if let Some(declared) = hole.declared
                && let Some(clash) = demanded
                    .iter()
                    .find(|t| !t.compatible_with(declared.required()))
            {
                return Some(format!(
                    "`{name}` is declared `{}` but used where a {} is required",
                    declared.label(),
                    clash.label()
                ));
            }
            let types: Vec<&str> = demanded.iter().map(|t| t.label()).collect();
            (types.len() > 1).then(|| format!("`{name}` is used as {}", types.join(" and as ")))
        })
        .collect();
    if bad.is_empty() {
        return Ok(());
    }
    Err(format!(
        "contradictory placeholder types: {}. No single placeholder value can satisfy the \
         document as written — correct the declaration, or use a separate placeholder name \
         per position.",
        bad.join("; ")
    ))
}

/// Whether the document mentions `!tag` anywhere. The loader represents a YAML
/// tag as a single-key map, so this is a plain structural walk — no grammar
/// knowledge, and nothing to keep in sync when a tag is added.
pub fn mentions_tag(value: &Json, tag: &str) -> bool {
    match value {
        Json::Object(map) => map
            .iter()
            .any(|(k, v)| k.trim_start_matches('!') == tag || mentions_tag(v, tag)),
        Json::Array(items) => items.iter().any(|v| mentions_tag(v, tag)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::param_type::ParamType;

    fn check(text: &str, kind: StrategyKind) -> anyhow::Result<CheckedSpec> {
        let value = crate::spec::input::parse_value(text).expect("parses as YAML");
        check_value(value, kind, &HashMap::new())
    }

    fn single(text: &str) -> anyhow::Result<CheckedSpec> {
        check(text, StrategyKind::Single)
    }

    /// The type story of one placeholder, as `(name, declared, demanded)`.
    fn story(c: &CheckedSpec) -> Vec<(&str, Option<&'static str>, Vec<&'static str>)> {
        c.holes
            .iter()
            .map(|h| {
                (
                    h.name.as_str(),
                    h.declared.map(ParamType::label),
                    h.demanded().iter().map(|t| t.label()).collect(),
                )
            })
            .collect()
    }

    const CROSSOVER: &str = "\
root: !pick { symbol: !param SYMBOL, freq: !param FREQ }
long:
  enter: !gt
    lhs: !sma { period: !param FAST }
    rhs: !value 0
";

    /// The case the whole pass exists for: every placeholder defaultless, no
    /// values bound. `load_document` refuses this — correctly, it is about to
    /// hand back something runnable — and that refusal covers every strategy
    /// written to be parameterised per run.
    #[test]
    fn a_document_of_nothing_but_unset_placeholders_checks() {
        let c = single(CROSSOVER).expect("checks with no params bound");
        assert_eq!(
            story(&c),
            [
                ("FAST", None, vec!["number"]),
                ("FREQ", None, vec!["frequency"]),
                ("SYMBOL", None, vec!["symbol"]),
            ]
        );
    }

    /// The same document through the ordinary loader, so the test above is
    /// pinning a *difference* rather than restating something already true.
    #[test]
    fn the_ordinary_loader_still_refuses_the_same_document() {
        let value = crate::spec::input::parse_value(CROSSOVER).expect("parses");
        let value = crate::spec::root::apply_default(value, StrategyKind::Single);
        let err = crate::spec::params::substitute(value, &HashMap::new())
            .expect_err("a defaultless placeholder has no value to substitute");
        assert!(
            format!("{err:#}").contains("`FAST` is not set"),
            "unexpected: {err:#}"
        );
    }

    /// Check mode relaxes exactly one thing — that a placeholder have a value.
    /// Everything the typed parse decides it still decides.
    #[test]
    fn a_bad_document_still_fails_around_the_holes() {
        for (name, text) in [
            (
                "unknown tag",
                "root: BTC\nlong:\n  enter: !nope { period: !param P }\n",
            ),
            (
                "misspelled field",
                "root: BTC\nlong:\n  enter: !gt { lhs: !sma { perioD: !param P }, rhs: !value 0 }\n",
            ),
            (
                "a Real where a Bool is required",
                "root: BTC\nlong:\n  enter: !sma { period: !param P }\n",
            ),
        ] {
            assert!(single(text).is_err(), "{name}: should not have checked");
        }
    }

    /// A placeholder two positions disagree about can never be satisfied by any
    /// value, whatever the caller passes — so it is a document error, not a
    /// pending input. Decidable here and nowhere else: the ordinary loader has
    /// already resolved every placeholder by the time the types are visible.
    #[test]
    fn a_placeholder_two_positions_contradict_is_refused() {
        let err = single(
            "root: !pick { symbol: !param X }\n\
             long:\n  enter: !gt { lhs: !sma { period: !param X }, rhs: !value 0 }\n",
        )
        .expect_err("no single value is both a symbol and a period");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("`X` is used as number and as symbol"),
            "unexpected: {msg}"
        );
    }

    /// A declaration is a claim about the value, so a position demanding
    /// something else is the same defect caught one step earlier — and
    /// attributable to the declaration rather than to a tie between two slots.
    #[test]
    fn a_declaration_the_document_contradicts_is_refused() {
        let err = single(
            "root: BTC\nlong:\n  enter: !gt\n    \
             lhs: !sma { period: !param { key: F, type: string } }\n    rhs: !value 0\n",
        )
        .expect_err("a string cannot be a period");
        assert!(
            format!("{err:#}")
                .contains("`F` is declared `string` but used where a number is required"),
            "unexpected: {err:#}"
        );
    }

    /// The declaration has to survive the pass that reports it.
    ///
    /// It is logged by `substitute_for_check`, not by the parse, so the two
    /// halves of the observation ledger are written at different moments — and
    /// the hermetic drain has to happen before *either*. Draining it around the
    /// parse instead (the obvious place, since that is what the guard wraps)
    /// silently threw every declaration away: `check` then reported each
    /// placeholder as merely inferred, and the contradiction above stopped
    /// being caught at all.
    #[test]
    fn a_declared_type_outlives_the_hermetic_drain() {
        let c = single(
            "root: !pick { symbol: !param { key: SYM, type: string } }\n\
             long:\n  enter: !gt { lhs: !sma { period: 20 }, rhs: !value 0 }\n",
        )
        .expect("checks");
        assert_eq!(story(&c), [("SYM", Some("string"), vec!["symbol"])]);
    }

    /// The ledger is a thread-local the parse appends to. A check that returns
    /// early on a parse error must still drain it, or the *next* check on that
    /// thread reports the failed document's placeholders as its own.
    #[test]
    fn a_failed_check_leaves_nothing_behind_for_the_next_one() {
        let _ = single("root: !pick { symbol: !param LEAKED }\nlong:\n  enter: !nope {}\n")
            .expect_err("unknown tag");
        let c = single(
            "root: !pick { symbol: !param KEPT }\n\
             long:\n  enter: !gt { lhs: !sma { period: 20 }, rhs: !value 0 }\n",
        )
        .expect("checks");
        assert_eq!(story(&c), [("KEPT", None, vec!["symbol"])]);
    }

    /// A fully determined document is built as well as parsed — the one check
    /// the typed parse structurally cannot make. `PAIRS_VOL_TARGET` in
    /// `tests/check_builds.rs` is the case that motivated it.
    #[test]
    fn a_determined_document_is_built() {
        let c =
            single("root: BTC\nlong:\n  enter: !gt { lhs: !sma { period: 20 }, rhs: !value 0 }\n")
                .expect("checks");
        assert!(c.built, "nothing was left undetermined");
        assert!(c.holes.is_empty());
    }

    /// The four skips, each one a case where building would report a *document*
    /// error for a document whose only gap is an input nobody supplied. A
    /// `check` that started failing on these would be worse than the panic the
    /// build was added to replace.
    #[test]
    fn the_build_is_skipped_for_anything_the_input_still_owes() {
        for (name, text) in [
            (
                "an author-written !undefined",
                "root: BTC\nlong:\n  enter: !gt { lhs: !sma { period: !undefined }, rhs: !value 0 }\n",
            ),
            (
                "a placeholder standing for a whole expression",
                "root: BTC\nlong:\n  enter: !gt { lhs: !param SIG, rhs: !value 0 }\n",
            ),
            (
                "an overlay column only real data supplies",
                "root: BTC\nlong:\n  enter: !gt { lhs: !get { key: funding }, rhs: !value 0 }\n",
            ),
            (
                "a root left to a single-series input",
                "long:\n  enter: !gt { lhs: !sma { period: 20 }, rhs: !value 0 }\n",
            ),
            (
                "a root still holding a placeholder",
                "root: !pick { symbol: !param SYM }\n\
                 long:\n  enter: !gt { lhs: !sma { period: 20 }, rhs: !value 0 }\n",
            ),
        ] {
            let c = single(text).unwrap_or_else(|e| panic!("{name}: should have checked: {e:#}"));
            assert!(!c.built, "{name}: the build should have been skipped");
        }
    }

    /// A placeholder inside a *deferred* template body — a basket's `score:`, a
    /// portfolio's `weights:` — is not reached by the typed parse. The body is
    /// held as a raw tree and re-parsed per symbol at **build** time, so the
    /// check-mode guard has to still be held when the build runs or the hole
    /// arrives there as the bare sentinel mapping and every scalar slot it sits
    /// in fails with an `invalid type: map`.
    ///
    /// Nothing else pins that the guard outlives the parse, and dropping it one
    /// line early makes every parameterised basket and portfolio unloadable —
    /// while leaving all four eagerly-parsed shapes green.
    #[test]
    fn a_placeholder_inside_a_template_body_survives_the_build() {
        let c = check(
            "universe: !any_of [BTC, ETH]\n\
             selection: !top_bottom { longs: 1, shorts: 0 }\n\
             score: !sma { period: !param LOOK }\nsizing: !value 1.0\n",
            StrategyKind::Basket,
        )
        .expect("a basket scored on an unset placeholder still checks");
        assert!(c.built, "nothing left it undetermined");
        assert_eq!(story(&c), [("LOOK", None, vec!["number"])]);
    }

    /// The build re-parses every template body, re-recording demands the parse
    /// already reported — into the same thread-local ledger nobody else drains.
    /// A check that *built* must leave it as empty as one that did not.
    #[test]
    fn a_check_that_built_leaves_nothing_behind_either() {
        let first = check(
            "universe: !any_of [BTC, ETH]\n\
             selection: !top_bottom { longs: 1, shorts: 0 }\n\
             score: !sma { period: !param LOOK }\nsizing: !value 1.0\n",
            StrategyKind::Basket,
        )
        .expect("checks");
        assert!(first.built);
        let c = single(
            "root: !pick { symbol: !param KEPT }\n\
             long:\n  enter: !gt { lhs: !sma { period: 20 }, rhs: !value 0 }\n",
        )
        .expect("checks");
        assert_eq!(story(&c), [("KEPT", None, vec!["symbol"])]);
    }

    /// The refinement lattice, which is the whole reason `Symbol` and
    /// `Frequency` can be real types rather than a side-table of hints.
    ///
    /// A refinement never contradicts what it refines — one value satisfies
    /// both, and the finer one is the binding constraint to report. Two
    /// *different* refinements of the same type do contradict: both are
    /// strings, and no string is both a ticker and a bar cadence.
    #[test]
    fn a_refinement_agrees_with_what_it_refines_and_not_with_its_sibling() {
        // `symbol:` is `SymbolName`, `stream:` a plain `String`. One name in
        // both is one ticker, and `symbol` is what to ask the caller for.
        let c = single(
            "root: !pick { symbol: !param X }\nlong:\n  enter: !gt\n    \
             lhs: !close { source: !pick { stream: !param X } }\n    rhs: !value 0\n",
        )
        .expect("a symbol is a string");
        assert_eq!(story(&c), [("X", None, vec!["symbol"])]);
        assert!(
            c.holes[0].used.contains(&RequiredType::Str),
            "the coarse demand is still recorded, just not reported: {:?}",
            c.holes[0].used
        );

        for (name, text) in [
            (
                "symbol and frequency",
                "root: !pick { symbol: !param X, freq: !param X }\nlong: { enter: !value true }\n",
            ),
            (
                "symbol and number",
                "root: !pick { symbol: !param X }\nlong:\n  \
                 enter: !gt { lhs: !sma { period: !param X }, rhs: !value 0 }\n",
            ),
        ] {
            let err = single(text)
                .err()
                .unwrap_or_else(|| panic!("{name}: should have been refused as contradictory"));
            assert!(
                format!("{err:#}").contains("contradictory placeholder types"),
                "{name}: {err:#}"
            );
        }
    }

    /// A declaration and a refined slot are one claim when the declaration is
    /// the *coarser* of the two. `type: string` on a `!pick { symbol: }` has
    /// been the documented way to keep a numeric ticker a string since before
    /// the refined types existed, and it must not become a contradiction.
    #[test]
    fn a_coarser_declaration_agrees_with_a_refined_slot() {
        let c = single(
            "root: !pick { symbol: !param { key: X, type: string } }\n\
             long: { enter: !value true }\n",
        )
        .expect("`string` is what `symbol` refines");
        assert_eq!(story(&c), [("X", Some("string"), vec!["symbol"])]);

        // The other direction still clashes: `symbol` is not a cadence.
        let err = single(
            "root: !pick { freq: !param { key: X, type: symbol } }\n\
             long: { enter: !value true }\n",
        )
        .expect_err("a ticker is not a bar cadence");
        assert!(
            format!("{err:#}")
                .contains("`X` is declared `symbol` but used where a frequency is required"),
            "unexpected: {err:#}"
        );
    }

    /// A hole has to answer with a value its own slot accepts, or the documents
    /// `check` exists for fail on the stand-in rather than on anything written.
    ///
    /// Regression, and a pre-existing one: a `FreqToken` hole answered the
    /// generic `""`, which reached `resolve_stream` at build and came back
    /// `invalid frequency ""` — so a document parameterising a cadence could not
    /// be checked at all. The same rule integers already followed (they answer
    /// `1`, not `0`, so a `NonZeroUsize` period parses); the refinements only
    /// needed it applied to them.
    #[test]
    fn a_refined_hole_answers_with_a_value_its_own_format_accepts() {
        for (name, text) in [
            (
                "a parameterised cadence",
                "root: BTC\nlong:\n  enter: !gt\n    \
                 lhs: !close { source: !pick { freq: !param F } }\n    rhs: !value 0\n",
            ),
            (
                "a parameterised symbol",
                "root: BTC\nlong:\n  enter: !gt\n    \
                 lhs: !close { source: !pick { symbol: !param S } }\n    rhs: !value 0\n",
            ),
            (
                "both at once",
                "root: BTC\nlong:\n  enter: !gt\n    \
                 lhs: !close { source: !pick { symbol: !param S, freq: !param F } }\n    \
                 rhs: !value 0\n",
            ),
        ] {
            let c = single(text).unwrap_or_else(|e| panic!("{name}: should have checked: {e:#}"));
            assert!(
                c.built,
                "{name}: nothing left it undetermined, so it must build"
            );
        }
    }

    /// An empty symbol names nothing, so the leaf reads `None` on every bar and
    /// the run reports a plausible zero-fill instead of failing — the failure
    /// mode `RootSpec::as_pick` exists to prevent.
    ///
    /// Refused at **build**, not at parse, and the second half of this test is
    /// why: a `check`-mode hole stands in for the value, so a parse-time
    /// rejection would refuse every document with an unset symbol placeholder.
    #[test]
    fn an_empty_symbol_is_refused_but_an_unset_one_is_not() {
        let err = single(
            "root: BTC\nlong:\n  enter: !gt\n    \
             lhs: !close { source: !pick { symbol: \"\" } }\n    rhs: !value 0\n",
        )
        .expect_err("an empty symbol names nothing");
        assert!(
            format!("{err:#}").contains("`symbol` is empty"),
            "unexpected: {err:#}"
        );

        let c = single(
            "root: BTC\nlong:\n  enter: !gt\n    \
             lhs: !close { source: !pick { symbol: !param S } }\n    rhs: !value 0\n",
        )
        .expect("an unset symbol is a value nobody passed, not a broken document");
        assert!(c.built);
    }

    /// A placeholder standing where a whole expression goes records `Expr`,
    /// which is not a *demand* — every scalar a caller can pass is an
    /// expression, so it constrains nothing and contradicts nothing.
    #[test]
    fn an_expression_placeholder_demands_nothing() {
        let c = single("root: BTC\nlong:\n  enter: !gt { lhs: !param SIG, rhs: !value 0 }\n")
            .expect("checks");
        let hole = &c.holes[0];
        assert_eq!(hole.name, "SIG");
        assert!(hole.demanded().is_empty(), "{:?}", hole.demanded());
        assert_eq!(hole.used, [RequiredType::Expr]);
    }

    /// A partially-bound table is the ordinary case for a caller that knows
    /// some values: a bound placeholder is substituted and type-checked
    /// normally, and only the rest become holes.
    #[test]
    fn binding_some_placeholders_leaves_only_the_rest_as_holes() {
        let value = crate::spec::input::parse_value(CROSSOVER).expect("parses");
        let params = HashMap::from([("FAST".to_string(), serde_json::json!(20))]);
        let c = check_value(value, StrategyKind::Single, &params).expect("checks");
        assert_eq!(
            story(&c),
            [
                ("FREQ", None, vec!["frequency"]),
                ("SYMBOL", None, vec!["symbol"])
            ]
        );
    }

    /// A bound value of the wrong type is still refused — binding a value opts
    /// that placeholder back into ordinary validation.
    #[test]
    fn a_bound_value_of_the_wrong_type_is_still_refused() {
        let value = crate::spec::input::parse_value(CROSSOVER).expect("parses");
        let params = HashMap::from([("FAST".to_string(), serde_json::json!("twenty"))]);
        assert!(
            check_value(value, StrategyKind::Single, &params).is_err(),
            "a string is not a period"
        );
    }

    /// `reads` is the same walk a loaded spec reports: with no data in hand
    /// `check` cannot say these series are *present*, only that they are
    /// *required*.
    #[test]
    fn the_series_a_document_reads_come_back_with_it() {
        let c = single(
            "root: BTC\nlong:\n  enter: !gt\n    \
             lhs: !close { source: !pick { symbol: ETH } }\n    rhs: !value 0\n",
        )
        .expect("checks");
        assert_eq!(c.reads, ["ETH"]);
    }

    /// The two spellings of the default `root:` are **not** interchangeable to
    /// a caller with no values, and the difference is `default: null`.
    ///
    /// Omitting `root:` splices [`root::default_tree`](super::root::default_tree),
    /// whose placeholders are *optional* — so they resolve to null, the `!pick`
    /// collapses to the sole-atom selector, and nothing is a hole. That document
    /// has always loaded with `params = {}`; it is not what `check` unblocked,
    /// and a form built on `holes` will not be offered a symbol box for it.
    ///
    /// Spelling the same root out in the bare canonical form — `!param SYMBOL`,
    /// no body — is a *required* placeholder, which `load_document` refuses and
    /// this reports, typed, as the two holes a caller has to fill.
    #[test]
    fn the_two_spellings_of_the_default_root_are_not_the_same_document() {
        const OMITTED: &str = "long:\n  enter: !value true\n";
        const SPELLED: &str = "root: !pick { symbol: !param SYMBOL, freq: !param FREQ }\n\
                               long:\n  enter: !value true\n";

        let omitted = single(OMITTED).expect("checks");
        assert_eq!(story(&omitted), [], "the spliced placeholders are optional");
        assert!(!omitted.built, "the sole-atom root is left to the input");
        // And the ordinary loader takes it too — so this spelling was never the
        // document `check` exists for.
        let value = crate::spec::input::parse_value(OMITTED).expect("parses");
        let value = super::super::root::apply_default(value, StrategyKind::Single);
        crate::spec::params::substitute(value, &HashMap::new())
            .expect("an optional placeholder needs no value");

        let spelled = single(SPELLED).expect("checks");
        assert_eq!(
            story(&spelled),
            [
                ("FREQ", None, vec!["frequency"]),
                ("SYMBOL", None, vec!["symbol"]),
            ]
        );
        let value = crate::spec::input::parse_value(SPELLED).expect("parses");
        crate::spec::params::substitute(value, &HashMap::new())
            .expect_err("the bare spelling is required, and the loader says so");
    }

    /// An omitted `root:` is defaulted for `Single` **only** — a `root:`-less
    /// document is structurally a `multi:` one, and `detect_kind` reads it that
    /// way. `check` inherits that, exactly as `load_document` does: a caller
    /// that means single-asset has to say which shape it meant.
    #[test]
    fn a_rootless_document_is_only_single_if_the_caller_says_so() {
        const TEXT: &str =
            "long:\n  enter: !gt { lhs: !sma { period: !param FAST }, rhs: !value 0 }\n";
        for (kind, built) in [(StrategyKind::Single, false), (StrategyKind::Multi, true)] {
            let c = check(TEXT, kind).unwrap_or_else(|e| panic!("{kind:?}: {e:#}"));
            assert_eq!(story(&c), [("FAST", None, vec!["number"])], "{kind:?}");
            assert_eq!(c.built, built, "{kind:?}: only single has a root to owe");
        }
    }

    /// A placeholder carrying a `default:` is *resolved*, not held — so it is
    /// not a hole and the report does not mention it, whatever it declared.
    ///
    /// Worth pinning because it bounds what a caller can build on `holes`: the
    /// report types the placeholders nobody has a value for, which is exactly
    /// the set with no default to read a type off. A form that wants every
    /// knob typed still has to read the defaulted ones from their defaults.
    #[test]
    fn a_placeholder_with_a_default_is_not_a_hole() {
        let c = single(
            "root: BTC\nlong:\n  enter: !gt\n    \
             lhs: !sma { period: !param { key: FAST, default: 10, type: integer } }\n    \
             rhs: !sma { period: !param SLOW }\n",
        )
        .expect("checks");
        assert_eq!(
            story(&c),
            [("SLOW", None, vec!["number"])],
            "the defaulted `FAST` resolved, declaration and all"
        );
    }

    /// The guard is thread-local and turning it on for a parse that is not a
    /// check would make an ordinary load hole-aware. It is RAII, so this holds
    /// on the error path too — which is the one a caller validating in a loop,
    /// or a pool worker reused across requests, hits first.
    #[test]
    fn check_mode_is_off_again_by_the_time_a_check_returns() {
        assert!(
            !undefined::in_check_mode(),
            "not in check mode to begin with"
        );

        single(CROSSOVER).expect("checks");
        assert!(
            !undefined::in_check_mode(),
            "left on after a check that passed"
        );

        let determined =
            "root: BTC\nlong:\n  enter: !gt { lhs: !sma { period: 20 }, rhs: !value 0 }\n";
        single(determined).expect("checks and builds");
        assert!(
            !undefined::in_check_mode(),
            "left on after a check that built"
        );

        single("root: BTC\nlong:\n  enter: !nope {}\n").expect_err("a bad tag");
        assert!(
            !undefined::in_check_mode(),
            "left on after a check that failed"
        );
    }

    /// Every shape goes through the one pass — `check` has no per-shape twin,
    /// and a sixth shape gets its arm in `parse_holed` alongside the rest.
    #[test]
    fn every_shape_checks_around_its_own_placeholders() {
        for (kind, text) in [
            (
                StrategyKind::Pairs,
                "left: !param A\nright: !param B\nlong_spread:\n  \
                 enter: !gt { lhs: !close { source: !pick { symbol: BTC } }, rhs: !value 0 }\n",
            ),
            (
                StrategyKind::Basket,
                "universe: !any_of [BTC, ETH]\nselection: !top_bottom { longs: 1, shorts: 0 }\n\
                 score: !sma { period: !param LOOK }\nsizing: !value 1.0\n",
            ),
            (
                StrategyKind::Multi,
                "long:\n  enter: !gt { lhs: !sma { period: !param P }, rhs: !value 0 }\n  \
                 exit: !lt { lhs: !sma { period: !param P }, rhs: !value 0 }\n",
            ),
            (
                StrategyKind::Portfolio,
                "children:\n  - name: a\n    strategy:\n      root: BTC\n      long:\n        \
                 enter: !gt { lhs: !sma { period: !param P }, rhs: !value 0 }\n",
            ),
        ] {
            let c = check(text, kind)
                .unwrap_or_else(|e| panic!("{kind:?}: should have checked: {e:#}"));
            assert!(!c.holes.is_empty(), "{kind:?}: the placeholders are holes");
        }
    }
}
