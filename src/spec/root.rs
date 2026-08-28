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
//! A pairs document's two legs default the same way, one [`RootKey`] each:
//!
//! ```yaml
//! left:  !pick { symbol: !param { key: LEFT  }, freq: !param { key: FREQ } }
//! right: !pick { symbol: !param { key: RIGHT }, freq: !param { key: FREQ } }
//! ```
//!
//! One `FREQ`, not two — a pair's legs are two series read off one snapshot
//! stream, and a document that wanted them at different cadences would be
//! spelling out both roots anyway. Neither leg is seeded from the input: which
//! of a two-symbol frame's entries is the *left* one is precisely what the
//! document was supposed to say, so an unset `LEFT` is a build error naming the
//! parameter rather than a guess off the frame's sort order.
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
        // Anything that isn't a plain string (an `!slot` hole, a nested
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

    /// The one symbol the [`RootKey`] this root came from names, or a build
    /// error.
    ///
    /// `key` is the document key — it spells itself in the message and knows
    /// which `--params` name fills it in, so a pair's absent leg says `left:`
    /// and `LEFT` rather than the single-asset shape's `root:` and `SYMBOL`.
    /// `shape` names the document shape. Both errors are values, reported with
    /// the rest of the `!tag > ` breadcrumb by the caller.
    pub fn sole_symbol(&self, key: RootKey, shape: &'static str) -> Result<String, String> {
        let named = self.named_symbols();
        let mut it = named.into_iter();
        match (it.next(), it.next()) {
            (Some(one), None) => Ok(one),
            (None, _) => Err(key.names_nothing()),
            (Some(a), Some(b)) => {
                let rest: Vec<String> = std::iter::once(a)
                    .chain(std::iter::once(b))
                    .chain(self.named_symbols().into_iter().skip(2))
                    .collect();
                Err(format!(
                    "`{}:` names {} symbols ({}); a {shape} document trades one there",
                    key.key,
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

/// A root-bearing **document key**, and the `--params` names its default
/// spelling reads out of.
///
/// Three of them, because two shapes carry a root and one of those carries two:
/// [`ROOT`](Self::ROOT) for a single-asset `root:`, [`LEFT`](Self::LEFT) and
/// [`RIGHT`](Self::RIGHT) for a pair's two legs. Each ties together the key as
/// written in YAML, the tree [`apply_default`] splices for it, and the error a
/// root of that key reports when it names no symbol — so those three can not
/// drift into disagreeing about what to pass.
///
/// The **cadence** name is deliberately not per-key. A pair's two legs trade off
/// one snapshot stream, so they share [`FREQ_PARAM`]; only the symbol differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootKey {
    /// The YAML key, as written in a document and printed in an error.
    pub key: &'static str,
    /// The `--params` name this key's default tree reads its symbol from.
    pub symbol_param: &'static str,
    /// Does the CLI fill this key's symbol in from a single-series input?
    /// True only for [`ROOT`](Self::ROOT) — see `seed_sole_symbol`.
    seeded_from_input: bool,
}

impl RootKey {
    /// A single-asset document's `root:`, reading `SYMBOL` / `FREQ`.
    pub const ROOT: Self = Self {
        key: "root",
        symbol_param: SYMBOL_PARAM,
        seeded_from_input: true,
    };
    /// A pairs document's `left:` leg, reading `LEFT` / `FREQ`.
    pub const LEFT: Self = Self {
        key: "left",
        symbol_param: LEFT_PARAM,
        seeded_from_input: false,
    };
    /// A pairs document's `right:` leg, reading `RIGHT` / `FREQ`.
    pub const RIGHT: Self = Self {
        key: "right",
        symbol_param: RIGHT_PARAM,
        seeded_from_input: false,
    };

    /// The untyped tree [`apply_default`] splices for this key. Public so a
    /// caller can show a user what an omitted key expands to, and so the tests
    /// and the published JSON Schema assert against the one definition rather
    /// than a copy of it.
    ///
    /// Both placeholders declare their type. The slots they sit in already
    /// demand it (`symbol:` is a `SymbolName`, `freq:` a `FreqToken`), so this
    /// adds nothing to a `check` report — what it adds is **coercion**, on the
    /// path where nothing else can: `--params SYMBOL=123` is parsed as a number
    /// by `params::scalar` and a numeric ticker stops being a symbol. Spelling
    /// the root out longhand with `type: string` has always been the fix; an
    /// implicit one had no way to ask for it. Same for `FREQ=1hh`, which now
    /// fails at load naming the parameter instead of at the build.
    pub fn default_tree(self) -> serde_json::Value {
        serde_json::json!({
            "pick": {
                "symbol": { "param": { "key": self.symbol_param, "default": null, "type": "symbol" } },
                "freq": { "param": { "key": FREQ_PARAM, "default": null, "type": "frequency" } },
            }
        })
    }

    /// The build error a root of this key reports when it names no symbol.
    ///
    /// Split by key because the way out differs: an unset `SYMBOL` is something
    /// a one-symbol `--series` frame can answer, and an unset `LEFT` is not —
    /// which of a two-symbol frame's entries is the left leg is exactly the
    /// question the document was supposed to answer.
    fn names_nothing(self) -> String {
        let Self {
            key, symbol_param, ..
        } = self;
        let mut message = format!(
            "`{key}:` names no symbol, so there is nothing to trade or to slice the input \
             by — write one (`{key}: BTCUSDT`), or pass `--params {symbol_param}=…`"
        );
        if self.seeded_from_input {
            message.push_str(
                ", or run against a single-series input, whose sole symbol the CLI fills in",
            );
        }
        message
    }
}

/// Splice a shape's default root keys into a document tree that omits them — a
/// full spec map, or a preset tag's payload.
///
/// Runs on the untyped tree *before* `!param` substitution, which is the whole
/// point: what it splices is the ordinary expression
///
/// ```yaml
/// !pick { symbol: !param { key: SYMBOL }, freq: !param { key: FREQ } }
/// ```
///
/// (with `LEFT` / `RIGHT` in place of `SYMBOL` for a pair's two legs), so every
/// placeholder resolves out of `--params` exactly like ones the author wrote,
/// under [`substitute`](super::params::substitute) and
/// [`substitute_for_check`](super::params::substitute_for_check) alike. Each
/// carries `default: null` rather than being required, and `desugar` drops a
/// null selector field — that is what makes "leave `SYMBOL` unset" mean *omit
/// the key* rather than *error*.
///
/// **The shape rule lives here, and only here.** `root:` is a key only the
/// single-asset shape (and its preset spelling) accepts, and `left:` / `right:`
/// only the pairs shape; every shape carries `deny_unknown_fields`, so splicing
/// the wrong key would turn a good document into a parse error. And the shape
/// cannot be recovered from the tree: once `root:` may be absent, a `single:`
/// document is structurally identical to a `multi:` one — both are a bare
/// `long:` / `short:` map. Hence the `kind` parameter. Callers pass the kind
/// they already hold (the CLI's shape prefix, Python's `kind=`) and never
/// re-implement the policy; [`load_document`](super::load_document) is the
/// loader that does it for them.
pub fn apply_default(
    value: serde_json::Value,
    kind: super::input::StrategyKind,
) -> serde_json::Value {
    let keys: &[RootKey] = match kind {
        super::input::StrategyKind::Single => &[RootKey::ROOT],
        super::input::StrategyKind::Pairs => &[RootKey::LEFT, RootKey::RIGHT],
        _ => return value,
    };
    let serde_json::Value::Object(mut map) = value else {
        // Not a document map at all (a bare scalar, a list). Whatever it is,
        // the typed parse owns the error message.
        return value;
    };
    // A preset arrives as a single-key map — `{buy_and_hold: {root: …}}` — so
    // the `root:` to default lives one level down, in the payload. Presets are
    // a single-asset spelling only, hence the kind check rather than a bare
    // recursion.
    if matches!(kind, super::input::StrategyKind::Single)
        && map.len() == 1
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
    for root_key in keys {
        if !map.contains_key(root_key.key) {
            map.insert(root_key.key.into(), root_key.default_tree());
        }
    }
    serde_json::Value::Object(map)
}

/// The `--params` name the single-asset default root reads its symbol from.
///
/// Exported because it is *ambient*: the CLI seeds it from a single-series
/// input, so the name has to be spelled identically in two places and this is
/// the one definition of it.
pub const SYMBOL_PARAM: &str = "SYMBOL";

/// The `--params` name a pairs document's default `left:` reads its symbol
/// from. See [`SYMBOL_PARAM`].
pub const LEFT_PARAM: &str = "LEFT";

/// The `--params` name a pairs document's default `right:` reads its symbol
/// from. See [`SYMBOL_PARAM`].
pub const RIGHT_PARAM: &str = "RIGHT";

/// The `--params` name every default root reads its cadence from — **one** name
/// across all three keys, because a pair's legs are two series off one stream.
/// See [`SYMBOL_PARAM`].
pub const FREQ_PARAM: &str = "FREQ";

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
            // …and, for a root that *is* the placeholder, report it — nothing
            // else on this path would. The hand-rolled node parse records only
            // that an *expression* goes here, which is true of every scalar and
            // so says nothing about what to pass. A bare root stands for a
            // symbol, so `Symbol` — the same thing this always asserted, now
            // that there is a type that spells it. See `undefined::observe_json`.
            //
            // Only for that shape. A root that is a *structure* around a hole —
            // `!pick { symbol: !param S }`, `!resample { every: !param N }` —
            // has every hole sitting in a field, and `NodeSpec`'s inner payload
            // parse routes back through the hole-aware deserializer, which types
            // each one from the field that demanded it. Painting the whole tree
            // `Str` on top of that made a numeric slot read as `number` *and*
            // `string`, and `check` refused the document as self-contradictory.
            if super::undefined::is_hole(&tree) {
                super::undefined::observe_json(&tree, super::undefined::RequiredType::Symbol);
            }
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
        assert_eq!(r.sole_symbol(RootKey::ROOT, "single").unwrap(), "BTCUSDT");
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
        let e = root("!pick {}")
            .sole_symbol(RootKey::ROOT, "single")
            .unwrap_err();
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
        let e = r.sole_symbol(RootKey::ROOT, "single").unwrap_err();
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
        assert_eq!(doc.get("root"), Some(&RootKey::ROOT.default_tree()));

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

    /// The default root's placeholders declare their types, and the point is
    /// **coercion** on the one path that had no other way to ask for it.
    ///
    /// `--params SYMBOL=123` is parsed as a *number* by `params::scalar`, and a
    /// numeric ticker then fails deserialization into the `symbol:` slot.
    /// Spelling the root out longhand with `type: string` has always been the
    /// documented fix; a document that never wrote a `root:` at all could not
    /// reach it. Declaring on the tree fugazi splices in closes that.
    #[test]
    fn the_default_root_coerces_a_numeric_ticker() {
        let doc = super::super::input::parse_value("long:\n  enter: !value true\n").expect("parse");
        let doc = apply_default(doc, StrategyKind::Single);
        let params = std::collections::HashMap::from([
            ("SYMBOL".to_string(), serde_json::json!(123)),
            ("FREQ".to_string(), serde_json::json!("1d")),
        ]);
        let resolved = super::super::params::substitute(doc, &params).expect("coerces");
        let spec: crate::spec::SingleStrategySpec =
            serde_json::from_value(resolved).expect("parses with a stringified ticker");
        assert_eq!(
            spec.root
                .sole_symbol(RootKey::ROOT, "single-asset")
                .expect("names one"),
            "123"
        );
    }

    /// The cadence half of the same declaration: a typo is refused at *load*,
    /// naming the parameter, instead of four layers down at the build.
    #[test]
    fn the_default_root_names_the_parameter_for_a_bad_cadence() {
        let doc = super::super::input::parse_value("long:\n  enter: !value true\n").expect("parse");
        let doc = apply_default(doc, StrategyKind::Single);
        let params = std::collections::HashMap::from([
            ("SYMBOL".to_string(), serde_json::json!("BTC")),
            ("FREQ".to_string(), serde_json::json!("1hh")),
        ]);
        let err = super::super::params::substitute(doc, &params).expect_err("`1hh` has no unit");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("parameter `FREQ`") && msg.contains("is not a bar cadence"),
            "unexpected: {msg}"
        );
    }

    /// A `null` default passes any declaration untouched.
    ///
    /// `null` is not a value of any type — it is how a body spells *resolves to
    /// absent*, which is exactly what the default `root:` means by it ("take
    /// the sole series from the input"). Coercing it made `default: null` and
    /// `type:` mutually exclusive, which broke every `root:`-less document the
    /// moment the default tree declared its types.
    #[test]
    fn a_null_default_is_not_coerced_against_the_declaration() {
        for ty in ["symbol", "frequency", "integer", "bool"] {
            let doc = super::super::input::parse_value(&format!(
                "root: BTC\nlong:\n  enter: !gt\n    \
                 lhs: !sma {{ period: !param {{ key: P, default: null, type: {ty} }} }}\n    \
                 rhs: !value 0\n"
            ))
            .expect("parse");
            assert!(
                super::super::params::substitute(doc, &std::collections::HashMap::new()).is_ok(),
                "`default: null` with `type: {ty}` must resolve to absent, not be coerced"
            );
        }
    }

    /// `check` substitutes through a different pass; the default's placeholders    /// `check` substitutes through a different pass; the default's placeholders
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

    /// A pairs document defaults **both** legs, off `LEFT` / `RIGHT` and one
    /// shared `FREQ`.
    #[test]
    fn an_omitted_pair_leg_is_defaulted_from_left_and_right() {
        let doc = super::super::input::parse_value("enter: !always\n").unwrap();
        let doc = apply_default(doc, StrategyKind::Pairs);
        assert_eq!(doc.get("left"), Some(&RootKey::LEFT.default_tree()));
        assert_eq!(doc.get("right"), Some(&RootKey::RIGHT.default_tree()));

        let params = [
            ("LEFT".to_string(), serde_json::json!("BTCUSDT")),
            ("RIGHT".to_string(), serde_json::json!("ETHUSDT")),
            ("FREQ".to_string(), serde_json::json!("4h")),
        ]
        .into_iter()
        .collect();
        let doc = super::super::params::substitute(doc, &params).unwrap();
        let left: RootSpec = serde_json::from_value(doc["left"].clone()).unwrap();
        let right: RootSpec = serde_json::from_value(doc["right"].clone()).unwrap();
        // One `FREQ` reaches both: a pair's legs are two series off one stream.
        assert_eq!(left.as_pick(), Some((Some("BTCUSDT"), Some("4h"))));
        assert_eq!(right.as_pick(), Some((Some("ETHUSDT"), Some("4h"))));
    }

    /// Half a pair is still half a pair — the leg that *was* named resolves,
    /// and the other collapses to the sole-atom root, which `try_build` refuses
    /// by name rather than trading whatever the snapshot happened to carry.
    #[test]
    fn an_unset_leg_leaves_the_sole_atom_root() {
        let params = [("LEFT".to_string(), serde_json::json!("BTC"))]
            .into_iter()
            .collect();
        let doc = super::super::params::substitute(
            apply_default(serde_json::json!({}), StrategyKind::Pairs),
            &params,
        )
        .unwrap();
        let left: RootSpec = serde_json::from_value(doc["left"].clone()).unwrap();
        let right: RootSpec = serde_json::from_value(doc["right"].clone()).unwrap();
        assert_eq!(left.as_pick(), Some((Some("BTC"), None)));
        assert_eq!(right.tree(), &serde_json::json!({"pick": {}}));
        let e = right.sole_symbol(RootKey::RIGHT, "pairs").unwrap_err();
        assert!(e.contains("`right:` names no symbol"), "{e}");
        assert!(e.contains("--params RIGHT=…"), "{e}");
        // …and *not* the single-asset way out: nothing seeds a leg from the
        // input, because which of two symbols is the left one is exactly what
        // the document was supposed to say.
        assert!(!e.contains("single-series"), "{e}");
    }

    #[test]
    fn an_explicit_leg_is_never_overwritten() {
        let doc = serde_json::json!({"left": "BTC", "right": "ETH", "enter": "always"});
        assert_eq!(apply_default(doc.clone(), StrategyKind::Pairs), doc);
        // Only the missing half is filled in.
        let doc = apply_default(
            serde_json::json!({"left": "BTC", "enter": "always"}),
            StrategyKind::Pairs,
        );
        assert_eq!(doc["left"], serde_json::json!("BTC"));
        assert_eq!(doc["right"], RootKey::RIGHT.default_tree());
    }

    /// The shape rule: every other shape carries `deny_unknown_fields`, so a
    /// spliced key it does not accept would turn a good document into a parse
    /// error.
    #[test]
    fn no_other_shape_is_spliced() {
        for kind in [
            StrategyKind::Basket,
            StrategyKind::Multi,
            StrategyKind::Portfolio,
        ] {
            let doc = serde_json::json!({"long": {"enter": "always"}});
            assert_eq!(apply_default(doc.clone(), kind), doc, "{kind:?}");
        }
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
        assert_eq!(doc["ma_crossover"]["root"], RootKey::ROOT.default_tree());
        // …and an unknown single-key map is not a preset, so it is defaulted as
        // the ordinary document map it is.
        let doc = apply_default(
            serde_json::json!({"long": {"enter": "always"}}),
            StrategyKind::Single,
        );
        assert_eq!(doc["root"], RootKey::ROOT.default_tree());
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
