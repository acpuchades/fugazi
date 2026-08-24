//! [`RootSpec`] — a document's **evaluation root**, and the static analysis that
//! recovers what it trades from an arbitrary expression.
//!
//! Every shape that names an instrument used to carry a bare `symbol: String`.
//! That one field silently did four jobs: it was the order-routing key, the
//! *blessed series* every `source:`-omitted leaf reads, the key the CLI sliced
//! the input frame by, and — through the mere presence of the key — the
//! discriminator telling a `single:` document from a `multi:` one.
//!
//! Only the second of those is really about *which series*, and pinning it to a
//! plain string meant a document could neither be swept over instruments nor say
//! anything at all about its own cadence. `root:` replaces it with an ordinary
//! atom-valued [`NodeSpec`] — the same vocabulary as every other slot — so
//! `!param` reaches it like anything else:
//!
//! ```yaml
//! root: BTCUSDT                                        # sugar for the line below
//! root: !pick { symbol: BTCUSDT }
//! root: !pick { symbol: !param { key: SYM, default: BTC }, freq: 4h }
//! root: !resample { every: 4, source: !pick { symbol: BTC } }
//! ```
//!
//! # The default root
//!
//! Omitting `root:` from a single-asset document is the same as writing
//!
//! ```yaml
//! root: !pick { symbol: !param { key: SYMBOL }, freq: !param { key: FREQ } }
//! ```
//!
//! — spliced in by [`apply_default`] *before* `!param` substitution, so those
//! two placeholders resolve out of `--params` like any the author wrote. Both
//! are **optional**: an unset one drops its key rather than erroring, so a
//! document with neither supplied falls all the way back to `!pick {}` — the
//! sole-atom unpack, which is exactly what a one-series input wants. The CLI
//! closes the last gap by seeding `SYMBOL` from a single-series `--series`
//! frame, so an omitted `root:` still yields a symbol to route orders through.
//!
//! # How an expression still answers a static question
//!
//! Three consumers read the traded symbol out of the document *before* anything
//! is built: [`StrategySpec::universe`](super::runnable::StrategySpec::universe)
//! and `declared_symbols` (which resolve per-symbol cost bundles and power the
//! "declared symbol not in the input at all" refusal), and the CLI's
//! `frame.atoms(&symbol)` slice. An arbitrary expression looks like it has
//! nothing to give them.
//!
//! It does — just not through the type. `!param` substitution happens at **load**,
//! before the typed parse, so by the time a `RootSpec` exists its tree is fully
//! resolved and the symbol is recoverable by a structural walk. That walk already
//! exists: [`reads::picked_symbols`](super::reads::picked_symbols), which collects
//! every `!pick { symbol: <string> }` and already knows to skip a non-string hole.
//! Sharing it is the point — `!pick` is the only tag that names an asset, so a new
//! tag cannot silently fall out of the analysis.
//!
//! What changes is *where* the constraint lives: from the grammar (a string field)
//! to the content (a build error naming what went wrong). Which is where this
//! crate puts bad input anyway — see the *Build errors are values* invariant.
//!
//! # Why the raw tree is kept
//!
//! [`NodeSpec`] is `Deserialize`-only, so a typed root cannot be walked without a
//! match over every one of its ~142 variants — a second table to keep in step with
//! the first, and exactly the drift this crate avoids. `RootSpec` therefore keeps
//! the [`serde_json::Value`] it parsed from alongside the typed node, and the
//! analysers walk the tree while the build path uses the node.

use std::collections::BTreeSet;

use serde::{Deserialize, Deserializer};

use super::expr::NodeSpec;

/// A document's evaluation root: an atom-valued expression naming the series
/// its `source:`-omitted leaves read, and — for the shapes that trade one
/// instrument — the instrument itself.
///
/// See the [module docs](self) for why both the typed node and the raw tree are
/// retained.
#[derive(Debug, Clone)]
pub struct RootSpec {
    /// The resolved untyped tree, kept for the static analysers.
    tree: serde_json::Value,
    /// The typed expression, kept for the build path.
    node: Box<NodeSpec>,
}

impl RootSpec {
    /// The root that names one series directly — `!pick { symbol, freq }`.
    ///
    /// The programmatic twin of the YAML spelling, for the callers that derive a
    /// root rather than read one: each leg of a basket / multi-asset shape
    /// (whose default root is its own symbol), a portfolio's single-asset child,
    /// and the overlay column grouper. Building through the same JSON the parser
    /// would have produced keeps one construction path, so the analysers cannot
    /// disagree with the builder.
    pub fn for_series(symbol: &str, freq: Option<&str>) -> Self {
        let mut body = serde_json::Map::new();
        body.insert("symbol".into(), serde_json::Value::String(symbol.into()));
        if let Some(f) = freq {
            body.insert("freq".into(), serde_json::Value::String(f.into()));
        }
        let tree = serde_json::json!({ "pick": serde_json::Value::Object(body) });
        let node = serde_json::from_value::<NodeSpec>(tree.clone())
            .expect("`!pick { symbol, freq }` is always a well-formed root");
        Self {
            tree,
            node: Box::new(node),
        }
    }

    /// [`for_series`](Self::for_series) with no cadence — the common case.
    pub fn for_symbol(symbol: &str) -> Self {
        Self::for_series(symbol, None)
    }

    /// The root that names nothing — `!pick {}`, the sole-atom unpack.
    ///
    /// What a document's [default root](apply_default) collapses to when
    /// neither `SYMBOL` nor `FREQ` is supplied: every `source:`-omitted leaf
    /// reads the one entry each snapshot carries, which is the whole of a
    /// single-series input. It names no *traded* symbol, so a shape that has to
    /// route an order still owes one — [`sole_symbol`](Self::sole_symbol) is
    /// where that is asked for.
    pub fn sole() -> Self {
        let tree = serde_json::json!({ "pick": serde_json::Map::new() });
        let node = serde_json::from_value::<NodeSpec>(tree.clone())
            .expect("`!pick {}` is always a well-formed root");
        Self {
            tree,
            node: Box::new(node),
        }
    }

    /// The expression to build. Handed to
    /// [`Root::blessed`](super::expr::Root::blessed).
    pub fn node(&self) -> &NodeSpec {
        &self.node
    }

    /// This root's `(symbol, freq)` when it is *exactly* a `!pick` — i.e. a
    /// plain selector, which is what a root overwhelmingly is.
    ///
    /// Lets the build path install the same `Pick::rooted` leaf it always did
    /// for that case, rather than routing a selector through the general
    /// expression build. Two reasons, and the first is correctness: the blessed
    /// root's documented semantics are *match, else fall back to the sole-atom
    /// unpack*, and only `Pick::rooted` has that fallback — going through
    /// `!pick`'s own build arm yields the strict `Pick::matching`, which reads
    /// `None` on the untagged size-1 snapshots the `Vec<Candle>` / `Vec<Atom>`
    /// drivers produce. The second is cost: a root is rebuilt once per
    /// `source:`-omitted leaf, and this keeps the common one a bare `Pick`
    /// instead of a payload round-trip per leaf.
    pub fn as_pick(&self) -> Option<(Option<&str>, Option<&str>)> {
        let body = self.tree.get("pick")?.as_object()?;
        // Anything that isn't a plain string (an `!arg` hole, a nested
        // expression) is not a selector this shortcut can answer for.
        let field = |k: &str| match body.get(k) {
            None => Some(None),
            Some(serde_json::Value::String(v)) => Some(Some(v.as_str())),
            Some(_) => None,
        };
        if body
            .keys()
            .any(|k| k != "symbol" && k != "freq" && k != "stream")
        {
            return None;
        }
        // Both spellings reach here unresolved; `expr::resolve_stream` is what
        // validates `freq` and refuses the pair, so this reports whichever is
        // present and lets the one checker do the checking.
        let stream = match (field("freq")?, field("stream")?) {
            (Some(f), None) => Some(f),
            (None, Some(s)) => Some(s),
            // Both named is a build error, not a selector — fall through to the
            // full build path so the error is raised there rather than here.
            (Some(_), Some(_)) => return None,
            (None, None) => None,
        };
        Some((field("symbol")?, stream))
    }

    /// Is this root still holding an unresolved placeholder?
    ///
    /// True only under `fugazi check`, which substitutes an unset required
    /// `!param` to a [hole](super::undefined) instead of erroring. Every other
    /// slot answers a hole with a typed zero and builds on regardless; a root
    /// cannot, because there is no symbol to route orders through — so `check`
    /// asks this before building rather than reporting a document error for a
    /// document whose only gap is a value nobody passed.
    pub fn has_hole(&self) -> bool {
        super::undefined::contains_hole(&self.tree)
    }

    /// The resolved untyped tree this root parsed from.
    pub fn tree(&self) -> &serde_json::Value {
        &self.tree
    }

    /// Every symbol this root names, via the shared `!pick` walk.
    ///
    /// Empty when the root names none — a root built entirely out of holes, or
    /// one that bottoms out in a bare leaf. That is not an error *here*; only a
    /// shape that must trade something turns it into one, through
    /// [`sole_symbol`](Self::sole_symbol).
    pub fn named_symbols(&self) -> BTreeSet<String> {
        super::reads::picked_symbols(&self.tree)
    }

    /// The one symbol a single-instrument shape trades, or a build error.
    ///
    /// `shape` names the document shape for the message. Both errors are values,
    /// reported with the rest of the `!tag > ` breadcrumb by the caller.
    pub fn sole_symbol(&self, shape: &'static str) -> Result<String, String> {
        let named = self.named_symbols();
        let mut it = named.into_iter();
        match (it.next(), it.next()) {
            (Some(one), None) => Ok(one),
            (None, _) => Err(
                "`root:` names no symbol, so there is nothing to trade or to \
                 slice the input by — write one (`root: !pick { symbol: BTCUSDT }`), \
                 pass `--params SYMBOL=…`, or run against a single-series input, \
                 whose sole symbol the CLI fills in"
                    .to_string(),
            ),
            (Some(a), Some(b)) => {
                let rest: Vec<String> = std::iter::once(a)
                    .chain(std::iter::once(b))
                    .chain(self.named_symbols().into_iter().skip(2))
                    .collect();
                Err(format!(
                    "`root:` names {} symbols ({}); a {shape} document trades one",
                    rest.len(),
                    rest.iter()
                        .map(|s| format!("`{s}`"))
                        .collect::<Vec<_>>()
                        .join(", "),
                ))
            }
        }
    }

    /// The bar cadence this root **declares**, when it says so plainly: a root
    /// that is exactly one `!pick` carrying a string `freq:`.
    ///
    /// Deliberately narrow. This feeds the CLI's cadence precedence chain
    /// (`-f/--frequency` → here → the `freq` column → gap detection), where the
    /// honest answer for a root too involved to read off is "this document does
    /// not declare one" — so a wrapped or computed root returns `None` and the
    /// chain behaves exactly as it did before `root:` existed. It degrades; it
    /// does not refuse.
    ///
    /// **This is the one place a [`StreamId`] is read back as a cadence, and it
    /// is why the field can be opaque everywhere else.** The returned string is
    /// a stream id, not a parsed `Frequency`; every caller finishes the job with
    /// `Frequency::from_str(f).ok()`, so a stream that names something other
    /// than a duration — `dollar-1e6`, a session id — simply yields `None` here
    /// and the precedence chain moves to the next rung. A duration is one
    /// parseable form of a stream id, and this is where the parse happens.
    ///
    /// [`StreamId`]: crate::types::StreamId
    pub fn declared_freq(&self) -> Option<&str> {
        // Only the *validated* spelling. A `stream:` promises no format, so
        // reading one here would put an unchecked string into the cadence
        // precedence chain — precisely what `freq:` exists to keep out of it.
        self.tree.get("pick")?.get("freq")?.as_str()
    }
}

/// The sole-atom root — see [`RootSpec::sole`]. This is what a `root:`-less
/// document lands on once its `SYMBOL` / `FREQ` placeholders resolve to
/// nothing, and it is also the `#[serde(default)]` every root-bearing field
/// carries, so a tree that never went through [`apply_default`] — a portfolio
/// child, a `!sharpe { strategy: … }` subtree, a hand-built
/// `serde_json::from_value` — defaults the same way.
impl Default for RootSpec {
    fn default() -> Self {
        Self::sole()
    }
}

/// Splice the default `root:` into a **single-asset** document tree that omits
/// one — a full spec map, or a preset tag's payload.
///
/// Runs on the untyped tree *before* `!param` substitution, which is the whole
/// point: what it splices is the ordinary expression
///
/// ```yaml
/// !pick { symbol: !param { key: SYMBOL }, freq: !param { key: FREQ } }
/// ```
///
/// so both placeholders resolve out of `--params` exactly like ones the author
/// wrote, under [`substitute`](super::params::substitute) and
/// [`substitute_for_check`](super::params::substitute_for_check) alike. Each
/// carries `default: null` rather than being required, and `desugar` drops a
/// null selector field — that is what makes "leave `SYMBOL` unset" mean *omit
/// the key* rather than *error*.
///
/// **The shape rule lives here, and only here.** `root:` is a key only the
/// single-asset shape (and its preset spelling) accepts; every other shape
/// carries `deny_unknown_fields`, so splicing one into a `basket:` document
/// would turn a good document into a parse error. And the shape cannot be
/// recovered from the tree: once `root:` may be absent, a `single:` document is
/// structurally identical to a `multi:` one — both are a bare `long:` /
/// `short:` map. Hence the `kind` parameter. Callers pass the kind they already
/// hold (the CLI's shape prefix, Python's `kind=`) and never re-implement the
/// policy; [`load_document`](super::load_document) is the loader that does it
/// for them.
pub fn apply_default(
    value: serde_json::Value,
    kind: super::input::StrategyKind,
) -> serde_json::Value {
    if !matches!(kind, super::input::StrategyKind::Single) {
        return value;
    }
    let serde_json::Value::Object(mut map) = value else {
        // Not a document map at all (a bare scalar, a list). Whatever it is,
        // the typed parse owns the error message.
        return value;
    };
    // A preset arrives as a single-key map — `{buy_and_hold: {root: …}}` — so
    // the `root:` to default lives one level down, in the payload.
    if map.len() == 1
        && let Some(tag) = map.keys().next().cloned()
        && super::preset::PRESET_TAGS.contains(&tag.as_str())
    {
        let payload = map.remove(&tag).expect("key just read");
        map.insert(
            tag,
            apply_default(payload, super::input::StrategyKind::Single),
        );
        return serde_json::Value::Object(map);
    }
    if !map.contains_key("root") {
        map.insert("root".into(), default_tree());
    }
    serde_json::Value::Object(map)
}

/// The `--params` name the default root reads its symbol from.
///
/// Exported because it is *ambient*: the CLI seeds it from a single-series
/// input, so the name has to be spelled identically in two places and this is
/// the one definition of it.
pub const SYMBOL_PARAM: &str = "SYMBOL";

/// The `--params` name the default root reads its cadence from. See
/// [`SYMBOL_PARAM`].
pub const FREQ_PARAM: &str = "FREQ";

/// The untyped tree [`apply_default`] splices. Public so a caller can show a
/// user what an omitted `root:` expands to, and so the tests assert against the
/// one definition rather than a copy of it.
pub fn default_tree() -> serde_json::Value {
    serde_json::json!({
        "pick": {
            "symbol": { "param": { "key": SYMBOL_PARAM, "default": null } },
            "freq": { "param": { "key": FREQ_PARAM, "default": null } },
        }
    })
}

/// Captures the untyped tree, then parses the typed node out of it.
///
/// Format-agnostic on purpose: a document reaches here as `serde_json::Value`
/// (the shape loaders' path) and a portfolio child as `serde_norway::Value`, and
/// both are self-describing, so buffering into a `serde_json::Value` first works
/// for either.
impl<'de> Deserialize<'de> for RootSpec {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;

        let tree = desugar(serde_json::Value::deserialize(d)?);
        let node = serde_json::from_value::<NodeSpec>(tree.clone()).map_err(D::Error::custom)?;
        // Under `fugazi check` an unset `!param` is held as a hole, and a hole
        // parses as a constant `Real` placeholder — which is not a bar, and is
        // not meant to be read as one. Demanding `atom` of it would turn every
        // `check` of a parameterized root into a false failure, so the demand
        // stands down exactly where the tree is admittedly incomplete.
        if super::undefined::contains_hole(&tree) {
            // …and report the placeholder, which nothing else on this path
            // would: the re-parse below is plain `serde_json`, outside the
            // hole-aware deserializer that records these. A root's placeholder
            // stands for a symbol, so `Str`. See `undefined::observe_json`.
            super::undefined::observe_json(&tree, super::undefined::RequiredType::Str);
        } else {
            require_atom(&node).map_err(D::Error::custom)?;
        }
        Ok(Self {
            tree,
            node: Box::new(node),
        })
    }
}

/// `root: BTCUSDT` → `root: !pick { symbol: BTCUSDT }`, and a null selector
/// field on a `!pick` root → that field omitted.
///
/// The string rewrite is needed because [`NodeSpec`]'s own bridge reads a bare
/// string as a *tag name* (`close` → `NodeSpec::Close`), so an unrewritten
/// symbol would come back as an unknown variant. Keeping the terse spelling
/// matters: it is what every document that used to say `symbol: BTCUSDT`
/// becomes, so the migration is a rename of the key and nothing else.
///
/// The null drop is what lets the [default root](apply_default) degrade
/// cleanly. `symbol: null` already *parses* — the field is an `Option` — but it
/// parses as a present, non-string entry, and [`as_pick`](RootSpec::as_pick)
/// answers `None` to those. That would cost the `Pick::rooted` fast path its
/// sole-atom fallback and turn an unset `SYMBOL` into a silent zero-fill run.
/// Removing the key instead leaves a root byte-identical to one the author
/// never wrote.
fn desugar(tree: serde_json::Value) -> serde_json::Value {
    match tree {
        serde_json::Value::String(sym) => serde_json::json!({ "pick": { "symbol": sym } }),
        serde_json::Value::Object(mut map) if map.len() == 1 && map.contains_key("pick") => {
            if let Some(serde_json::Value::Object(body)) = map.get_mut("pick") {
                body.retain(|_, v| !v.is_null());
            }
            serde_json::Value::Object(map)
        }
        other => other,
    }
}

/// Refuse a root that provably yields something other than a bar.
///
/// A root is the atom source every `source:`-omitted leaf reads, so a real- or
/// bool-valued one is a bad document. Follows the crate's typecheck convention:
/// an *undecidable* output ([`output_type`](super::typecheck::output_type)
/// returning `None`) is skipped, never rejected — only a type it can prove wrong
/// earns the error.
fn require_atom(node: &NodeSpec) -> Result<(), String> {
    use crate::runtime::PayloadType;
    match super::typecheck::output_type(node) {
        Some(PayloadType::Atom) | None => Ok(()),
        Some(other) => Err(format!(
            "`root:` must name a series, but this expression yields {other:?} — \
             a root is the bar every `source:`-omitted leaf reads"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::input::StrategyKind;

    fn root(yaml: &str) -> RootSpec {
        let v = super::super::input::parse_value(yaml).expect("parse");
        serde_json::from_value(v).expect("build root")
    }

    #[test]
    fn bare_string_desugars_to_a_pick() {
        let r = root("BTCUSDT");
        assert_eq!(
            r.tree(),
            &serde_json::json!({"pick": {"symbol": "BTCUSDT"}})
        );
        assert_eq!(r.sole_symbol("single").unwrap(), "BTCUSDT");
    }

    #[test]
    fn bare_string_and_explicit_pick_agree() {
        assert_eq!(
            root("BTCUSDT").tree(),
            root("!pick { symbol: BTCUSDT }").tree()
        );
    }

    #[test]
    fn a_non_atom_root_is_refused() {
        // `!close` yields a Real, not a bar. Caught at parse, before any build.
        let v = super::super::input::parse_value("!close").expect("parse");
        let e = serde_json::from_value::<RootSpec>(v)
            .unwrap_err()
            .to_string();
        assert!(e.contains("must name a series"), "{e}");
    }

    #[test]
    fn a_root_naming_nothing_is_an_error() {
        // A bare `!pick {}` is a legal atom source (the sole-entry unpack), but
        // it names nothing for the CLI to slice the frame by.
        let e = root("!pick {}").sole_symbol("single").unwrap_err();
        assert!(e.contains("names no symbol"), "{e}");
    }

    /// `!pick` is currently the only atom-output tag and cannot nest another,
    /// so no *parseable* root reaches this branch today. It is here for the
    /// vocabulary growing a composite atom source later — the shape's "trades
    /// one instrument" rule should already be written down when it does.
    #[test]
    fn a_root_naming_two_symbols_is_an_error() {
        let r = RootSpec {
            tree: serde_json::json!({
                "some_future_tag": {
                    "lhs": {"pick": {"symbol": "BTC"}},
                    "rhs": {"pick": {"symbol": "ETH"}},
                }
            }),
            node: Box::new(
                serde_json::from_value(serde_json::json!({"pick": {"symbol": "BTC"}})).unwrap(),
            ),
        };
        let e = r.sole_symbol("single").unwrap_err();
        assert!(e.contains("names 2 symbols"), "{e}");
        assert!(e.contains("`BTC`") && e.contains("`ETH`"), "{e}");
    }

    /// The blessed root must keep `Pick::rooted`'s *match, else sole-atom
    /// unpack* fallback, not the strict `Pick::matching` that `!pick`'s own
    /// build arm produces.
    ///
    /// Regression: routing every root through the general expression build cost
    /// that fallback, and any tag that drives a sub-chain over **untagged**
    /// synthesized bars — `!resample` feeding its `inner:`, the `Vec<Candle>` /
    /// `Vec<Atom>` drivers — then read `None` on every bar. It silently
    /// produced a zero-fill backtest, which is the failure this crate goes out
    /// of its way to make impossible.
    #[test]
    fn a_plain_selector_root_is_recognised_for_the_rooted_fast_path() {
        assert_eq!(root("BTC").as_pick(), Some((Some("BTC"), None)));
        assert_eq!(
            root("!pick { symbol: BTC, freq: 4h }").as_pick(),
            Some((Some("BTC"), Some("4h")))
        );
        assert_eq!(root("!pick {}").as_pick(), Some((None, None)));
    }

    #[test]
    fn declared_freq_reads_a_plain_pick() {
        assert_eq!(
            root("!pick { symbol: BTC, freq: 4h }").declared_freq(),
            Some("4h")
        );
        assert_eq!(root("!pick { symbol: BTC }").declared_freq(), None);
    }

    #[test]
    fn an_omitted_root_is_defaulted_from_symbol_and_freq() {
        let doc = super::super::input::parse_value("long:\n  enter: !always\n").unwrap();
        let doc = apply_default(doc, StrategyKind::Single);
        assert_eq!(doc.get("root"), Some(&default_tree()));

        let params = [
            ("SYMBOL".to_string(), serde_json::json!("BTCUSDT")),
            ("FREQ".to_string(), serde_json::json!("4h")),
        ]
        .into_iter()
        .collect();
        let doc = super::super::params::substitute(doc, &params).unwrap();
        let r: RootSpec = serde_json::from_value(doc["root"].clone()).unwrap();
        assert_eq!(r.as_pick(), Some((Some("BTCUSDT"), Some("4h"))));
        assert_eq!(r.declared_freq(), Some("4h"));
    }

    /// The half the whole design turns on: an unset `SYMBOL` / `FREQ` must
    /// *drop its key*, not error and not leave a null behind — a null would
    /// cost `as_pick` its answer, and with it `Pick::rooted`'s sole-atom
    /// fallback, which is a silent zero-fill run rather than a message.
    #[test]
    fn an_unset_symbol_or_freq_leaves_the_sole_atom_root() {
        let doc = apply_default(serde_json::json!({}), StrategyKind::Single);
        let doc = super::super::params::substitute(doc, &Default::default()).unwrap();
        let r: RootSpec = serde_json::from_value(doc["root"].clone()).unwrap();
        assert_eq!(r.tree(), &serde_json::json!({"pick": {}}));
        assert_eq!(r.as_pick(), Some((None, None)));
        assert!(r.named_symbols().is_empty());

        // Only one of the two supplied is the same story, one key down.
        let params = [("SYMBOL".to_string(), serde_json::json!("ETH"))]
            .into_iter()
            .collect();
        let doc = super::super::params::substitute(
            apply_default(serde_json::json!({}), StrategyKind::Single),
            &params,
        )
        .unwrap();
        let r: RootSpec = serde_json::from_value(doc["root"].clone()).unwrap();
        assert_eq!(r.tree(), &serde_json::json!({"pick": {"symbol": "ETH"}}));
        assert_eq!(r.as_pick(), Some((Some("ETH"), None)));
    }

    /// `check` substitutes through a different pass; the default's placeholders
    /// carry a `default:`, so they resolve there too rather than becoming holes
    /// that would make every `root:`-less document look under-specified.
    #[test]
    fn the_default_root_resolves_under_check_substitution_too() {
        let (doc, holes) = super::super::params::substitute_for_check(
            apply_default(serde_json::json!({}), StrategyKind::Single),
            &Default::default(),
        )
        .unwrap();
        assert_eq!(holes, 0);
        let r: RootSpec = serde_json::from_value(doc["root"].clone()).unwrap();
        assert_eq!(r.tree(), &serde_json::json!({"pick": {}}));
    }

    #[test]
    fn an_explicit_root_is_never_overwritten() {
        let doc = serde_json::json!({"root": "BTC", "long": {"enter": "always"}});
        assert_eq!(apply_default(doc.clone(), StrategyKind::Single), doc);
    }

    /// A preset carries its `root:` one level down, inside the tag's payload.
    #[test]
    fn a_preset_is_defaulted_inside_its_payload() {
        let doc = apply_default(
            serde_json::json!({"ma_crossover": {"fast": 3, "slow": 8}}),
            StrategyKind::Single,
        );
        assert_eq!(doc["ma_crossover"]["root"], default_tree());
        // …and an unknown single-key map is not a preset, so it is defaulted as
        // the ordinary document map it is.
        let doc = apply_default(
            serde_json::json!({"long": {"enter": "always"}}),
            StrategyKind::Single,
        );
        assert_eq!(doc["root"], default_tree());
    }

    #[test]
    fn the_serde_default_is_the_sole_atom_root() {
        assert_eq!(RootSpec::default().tree(), &serde_json::json!({"pick": {}}));
        assert_eq!(RootSpec::sole().as_pick(), Some((None, None)));
    }

    #[test]
    fn declared_freq_declines_a_root_that_does_not_say() {
        // Honest `None` rather than a guess — the cadence chain then behaves
        // exactly as it did before `root:` existed.
        assert_eq!(root("BTCUSDT").declared_freq(), None);
        assert_eq!(root("!pick {}").declared_freq(), None);
    }
}
