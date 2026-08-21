//! The machine-readable **grammar descriptor** — one JSON-serializable record
//! per YAML tag, reflected straight off the serde variant definitions.
//!
//! [`spec_tags`](crate::spec::typecheck::known_node_tags) already reads the tag
//! *names* off serde's own variant list so it cannot go stale. This module
//! carries the same anti-drift guarantee one level deeper: for every tag that
//! *is* a serde variant, names, shapes, fields, defaults, and prose all flow
//! from the `#[derive(SpecGrammar)]` on
//! [`NodeSpec`](crate::spec::NodeSpec),
//! [`SelectionRuleSpec`](crate::spec::basket::SelectionRuleSpec), and
//! [`UniverseSpec`](crate::spec::basket::UniverseSpec), so downstream consumers
//! (fugazi's own Python constructors, editor tooling, docs, the web service's
//! grammar table) generate from one authority instead of re-encoding each tag
//! by hand.
//!
//! The one exception is the load-time `weighting` / `document` tags
//! (`!fixed`/`!equal_weight`/`!import`/`!param`/`!arg`/`!undefined`): these are
//! never serde variants — a `Value` pass rewrites each away before the typed
//! parse — so `document_grammar_tags` hand-authors their records. Even there
//! the *name set* is pinned to the parser's own
//! [`REWRITTEN_TAGS`](crate::spec::typecheck::REWRITTEN_TAGS) by a test, so a
//! new load-time tag can't ship without a row.
//!
//! The derive fills every field except the four that serde cannot know —
//! `kind`, `output`, `since`, and any **alternate spelling** — which are
//! declared next to each variant via
//! `#[grammar(kind = "…", output = "…", since = "…", alt = "…")]` (see the
//! derive in the `fugazi-derive` crate). `kind` is mandatory: a new variant
//! fails to compile until it is classified, the same "every tag is a decision"
//! discipline the Python parity test enforces.
//!
//! `alt` is there because a tag's YAML shape is not always its variant's shape.
//! `!changed <node>` is a newtype variant that also parses as
//! `!changed { source: <node> }`; `!unstable { source }` is a struct variant
//! that also parses with its inner written bare. Those spellings live in
//! `NodeSpec::parse_unchecked`'s normalisation pass, which the derive cannot
//! read — so they are declared, and `tests/spec_grammar.rs` settles the claim
//! against the parser in both directions: every declared form must parse, and
//! no unary wrapper may accept a mirror spelling it hasn't declared.

use serde::Serialize;

/// Bumped when the *shape* of the descriptor changes (a field added/removed/
/// renamed, or a legend value's meaning changes) so downstream consumers can
/// guard. Adding a *tag* does not bump this; renaming a tag's `name` is a
/// breaking change tracked by `since` instead.
///
/// - v1 (0.47): initial descriptor.
/// - v2 (0.48): added the positional `payload`; `literal` joined the field-type
///   vocabulary; `spec_json_schema()` shipped.
/// - v3 (0.51): added [`GrammarTag::category`], the fine conceptual sub-group
///   (a new field ⇒ a record-shape change ⇒ a bump). Consumers keyed on the
///   old shape keep working — the field is additive — but a generator that
///   hard-guards on the version needs to accept 3.
/// - v4 (0.61): added `GrammarField::node_output` and `payload_output` — the
///   output type each expression slot demands, the first part of the descriptor
///   sourced from `check`'s type table rather than reflected off serde. Both
///   are omitted when absent, so a v3 consumer reads an unchanged record.
/// - v5 (0.67): **`shape` / `fields` / `payload` / `payload_output` moved off
///   the tag and onto [`GrammarTag::forms`]**, a list. A tag's surface grammar
///   is a *set* of alternative spellings — `!param NAME` and
///   `!param { key, default }`, `!changed <node>` and `!changed { source }`,
///   `!unstable { source }` and bare `!unstable <node>` — and a single `shape`
///   could only ever name one of them. Eight tags were mis-described that way,
///   four of them in the reflected `node` group. `forms[0]` is the **canonical**
///   spelling (what a generator should emit); the rest are alternates a parser
///   also accepts. This is a breaking record-shape change: a consumer reading
///   `tag["shape"]` must move to `tag["forms"][0]["shape"]` and, if it validates
///   or completes, iterate all of `forms`.
/// - v6 (0.68): added [`GrammarTag::host_affecting`] — `true` only for
///   `import`, `false` on every other tag. Every record now carries it (not
///   omitted when `false`), so a v5 consumer reading a fixed field set sees a
///   new key rather than a missing one.
/// - v7 (unreleased): [`GrammarField::default`] became a **tagged**
///   [`GrammarDefault`] — `{ "literal": 12 }` or `{ "expr": "!close" }`, `null`
///   for no default. It used to be a bare JSON value doing double duty: a
///   literal for the 34 scalar keys that have one, and `null` for everything
///   else — which conflated "no default" with "there is one, but it isn't a
///   JSON literal". The latter was 69 fields whose default is a *node*
///   (`!ema`'s `source` → `!close`, `!atr`'s → `!current`, a selection rule's
///   `of` → `!everything`), and the only way to reach the fact was to regex
///   English out of the field's `doc`. An `expr` is always a bare leaf — the
///   *root floor*, carrying its own series root implicitly, so no fragment
///   nests and a leaf's own `source:` is honestly `null`. Breaking: a consumer
///   reading `field["default"]` as a value must read
///   `field["default"]["literal"]`.
///
/// **Not** a bump: 0.50 added the `universe` / `weighting` / `document` groups
/// (and the `none` output, `str_list` / `number_list` field types). New *rows*
/// and new *legend values* don't change the record *shape*, so that stayed at 2
/// — a bump would trip downstream version guards for no shape change. A consumer
/// with an exhaustive `group` / `kind` / `scope` switch should treat unknown
/// values as inert, not as an error.
pub const SCHEMA_VERSION: u32 = 7;

/// The `since` stamped on every tag that shipped at or before this release —
/// the baseline. Tags added afterwards carry their own real `since` via
/// `#[grammar(since = "X.Y")]`. See the module docs.
pub const SINCE_BASELINE: &str = "0.46";

/// What omitting an optional key gets you — the value of
/// [`GrammarField::default`].
///
/// Two cases, **tagged** rather than inferred, because a bare JSON value cannot
/// distinguish them: `{ "expr": "!close" }` and a field whose default is the
/// *string* `"close"` would both be `"close"`, and the field-type vocabulary
/// has `str` / `str_operand`, so that collision is expressible today.
///
/// - [`Literal`](Self::Literal) — a scalar key with a serde `default`. 34
///   fields: `!macd_line`'s `fast: 12`, `!bb_upper`'s `k: 2.0`.
/// - [`Expr`](Self::Expr) — a slot whose default is a **node**, reported as the
///   YAML fragment writing it produces. 69 fields: `!ema`'s `source` is
///   `!close`, `!atr`'s `!current`, `!donchian_upper`'s `high` / `low` are
///   `!high` / `!low`, a selection rule's `of` is `!everything`. It parses in
///   the slot it describes, so a consumer can both *display* it
///   (`!macd_line · source=!close, fast=12`) and *insert* it.
///
/// **An `Expr` is a root floor.** Every one is a leaf written bare, and a bare
/// leaf reads the blessed series its enclosing document confers — so the
/// fragment bottoms out there and never nests: `!ema`'s `source` is `!close`,
/// not `!close { source: … }`.
///
/// That floor is also why the **third case is `None`** and stays that way. A
/// leaf's *own* `source:` (`!close`, `!high`, `!equity`, …) defaults to "the
/// strategy's own series", which no tag names — it is the blessed series the
/// document's shape confers (see the blessed-series table in `CLAUDE.md`, and
/// [`Pick::rooted`](crate::indicators::Pick::rooted)), reachable by omission
/// only. Inventing a spelling for it would be worse than reporting nothing, and
/// nothing is lost by stopping a rung above it: the floor already says it.
///
/// An `Expr` is reflected off the default's own value
/// (`grammar::default_expr_of`), never off the field's prose, and settled
/// against the parser: `tests/spec_grammar.rs` parses the tag with the key
/// omitted and with the key set to the fragment, and requires the two to build
/// the same node.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GrammarDefault {
    /// A JSON literal, for a scalar key: `{ "literal": 12 }`.
    Literal(serde_json::Value),
    /// A YAML fragment, for a key whose default is a node:
    /// `{ "expr": "!close" }`.
    Expr(String),
}

impl GrammarDefault {
    /// The JSON literal, if this default is one.
    pub fn literal(&self) -> Option<&serde_json::Value> {
        match self {
            GrammarDefault::Literal(v) => Some(v),
            GrammarDefault::Expr(_) => None,
        }
    }

    /// The YAML fragment, if this default is one.
    pub fn expr(&self) -> Option<&str> {
        match self {
            GrammarDefault::Expr(e) => Some(e),
            GrammarDefault::Literal(_) => None,
        }
    }
}

/// One field of a `map`-shaped tag (the YAML body's keys). Empty for
/// `unit` / `newtype` / `seq` tags, which have no named fields.
#[derive(Debug, Clone, Serialize)]
pub struct GrammarField {
    /// The YAML key, exactly as written under the tag.
    pub name: String,
    /// The value's grammar type: `node` (a nested expression) · `node_list` ·
    /// `str_list` · `number_list` · `uint` · `number` · `str` · `bool` ·
    /// `strategy` (an embedded strategy document) · `match_cases` ·
    /// `str_operand`. A closed vocabulary the derive maps each Rust field type
    /// onto.
    #[serde(rename = "type")]
    pub ty: String,
    /// `false` when the key may be omitted — either the Rust field is an
    /// `Option`, or it carries a serde `default`.
    pub required: bool,
    /// What omitting the key is **equivalent to writing** — a JSON literal for
    /// a scalar (`!macd_line`'s `fast` is `{ "literal": 12 }`), a YAML fragment
    /// for a slot whose default is a node (`!ema`'s `source` is
    /// `{ "expr": "!close" }`). `null` means the field has **no expressible
    /// default**, which is a real answer rather than a gap: see
    /// [`GrammarDefault`] for why the two cases are tagged and what the third
    /// one is.
    pub default: Option<GrammarDefault>,
    /// For an expression-holding field (`ty` = `node` / `node_list` /
    /// `match_cases`), what the nested expression must **produce** — the
    /// `output` legend value(s) a filler is allowed to have. `!and`'s `lhs` is
    /// `["bool"]`, `!sma`'s `source` `["scalar"]`, `!atr`'s `["candle"]`,
    /// `!changed`'s `["bool", "scalar"]` (either is accepted).
    ///
    /// Three states, matching
    /// [`slot_demand`](crate::spec::typecheck::slot_demand)'s three answers:
    /// absent (`None`) for a field that holds no free expression — a scalar
    /// field, or a *book selector* like `!drawdown`'s `source`, which takes
    /// only `!strategy_book` / `!portfolio_book`; `[]` for a **passthrough**
    /// that demands nothing (`!unstable`'s `source`, `!resample`'s `inner`);
    /// otherwise the admitted set.
    ///
    /// This is the one part of the descriptor that is *not* reflected off
    /// serde — the type discipline lives in `check`'s own table, which this
    /// reads. It closes the gap `fugazi schema` documents as "structure only".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_output: Option<Vec<String>>,
    /// The field's `///` doc, if any.
    pub doc: Option<String>,
}

/// One **spelling** a tag accepts — its YAML shape and whatever that shape
/// carries.
///
/// A tag's surface grammar is a *set* of these, not one: `!param NAME` and
/// `!param { key: NAME, default: 8 }` are the same tag written two ways, and
/// only the second can carry a default. Before v5 the descriptor reported a
/// single `shape` per tag, which was silently wrong for eight of them — four in
/// the reflected `node` group, where the alternate spelling lives in
/// `NodeSpec::parse_unchecked`'s normalisation pass rather than in the variant,
/// so the derive could not see it. A consumer that validates, completes, or
/// scaffolds must iterate every form; one that only ever emits reads
/// [`GrammarTag::canonical`].
#[derive(Debug, Clone, Serialize)]
pub struct GrammarForm {
    /// How this spelling is written in YAML: `unit` (bare `!foo`) · `newtype`
    /// (`!foo <x>`) · `seq` (`!foo [ … ]`) · `map` (`!foo { … }`).
    pub shape: String,
    /// The `map` body's keys; empty for the other shapes.
    pub fields: Vec<GrammarField>,
    /// The grammar type of the single positional value a `newtype` / `seq`
    /// spelling carries (which has no named `fields`): `node` for
    /// `!not <node>`, `uint` for `!every <n>`, `node_list` for `!all [ … ]`,
    /// `literal` for `!value <x>`. `None` for `unit` / `map`.
    pub payload: Option<String>,
    /// [`GrammarField::node_output`] for the positional [`payload`](Self::payload)
    /// — `["bool"]` for `!not` / `!all` / `!any`, `["bool", "scalar"]` for
    /// `!changed`. Absent unless `payload` is `node` / `node_list`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_output: Option<Vec<String>>,
    /// Where this spelling may be written, when that is narrower than "wherever
    /// the tag's `group` is accepted". Absent (the common case) means
    /// unrestricted: every `node` / `selection` form, and the position-free
    /// load-time placeholders `!param` / `!import`, whose `Value`-tree passes
    /// rewrite them in *any* value position — an expression slot, a scalar field
    /// like `period:`, a string field like `symbol:`, a list element.
    ///
    /// The closed vocabulary, all of it on `document` / `weighting` tags:
    ///
    /// - `template` — only inside a deferred [`SpecTemplate`](crate::spec::SpecTemplate)
    ///   body (a basket's `score:` / `sizing:`, a multi-asset side's `enter:`, a
    ///   portfolio's `weights:`). `!arg` is resolved at *build* time by
    ///   `args::substitute`, which runs nowhere else — outside a template it is
    ///   a hard parse error, under `check` too. **`group == "document"` is a
    ///   provenance label, not a position claim**; this is the field that says
    ///   where.
    /// - `portfolio_weights` — only at the top level of a portfolio `weights:`
    ///   template, where `rewrite_weights_sugar` runs.
    /// - `internal` — never authored by hand. `!undefined` is a check-mode
    ///   stand-in; a runnable document carrying one is refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// What this spelling means, when the tag's own prose doesn't already say
    /// it. Required on every non-canonical form (a test pins it): an alternate
    /// exists because it does something the canonical one can't — carry a
    /// `default`, name a different asset, mean 1/N instead of a literal weight
    /// — and that difference is exactly what a completion engine has to show.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
}

/// One tag's full grammar record. See the module docs for the legend.
#[derive(Debug, Clone, Serialize)]
pub struct GrammarTag {
    /// Variant name, without the leading `!`. A public contract — downstream
    /// codegens off it and anchors docs at `#tag-<name>`. Identical to the
    /// corresponding entry of `spec_tags()`.
    pub name: String,
    /// Which vocabulary the tag belongs to: `node` (the composable expression
    /// enum) · `selection` (a `basket:` document's `selection:` rules) ·
    /// `universe` (`!all_of`/`!any_of`) · `weighting` (portfolio `weights:`
    /// sugar) · `document` (load-time `!import`/`!param`/`!arg`/`!undefined`).
    /// Only `node` and `selection` are slot-fillable expressions; the other
    /// three are document-level directives. Consumers that filtered on
    /// `group == "node"` before these were added keep working unchanged.
    pub group: String,
    /// `source` · `indicator` · `operator` · `predicate` · `function` ·
    /// `selection` · `universe` · `weighting` · `document`. The semantic family;
    /// declared per variant (or per hand-authored row).
    pub kind: String,
    /// Every spelling the tag accepts, **canonical first** — never empty.
    ///
    /// `forms[0]` is what a generator should emit and what
    /// [`canonical`](Self::canonical) returns; `forms[1..]` are alternates a
    /// parser also takes, each carrying its own `doc`. Most tags have exactly
    /// one. See [`GrammarForm`] for why this is a list and not a `shape`.
    pub forms: Vec<GrammarForm>,
    /// What the tag evaluates to: `scalar` (a `Real`) · `bool` · `str` ·
    /// `time` · `candle` · `atom` · `book` · `any` (schema- or
    /// operand-dependent) · `selection` (a `selection`-group rule) · `struct` ·
    /// `none` (a document-level directive that resolves to another node, or
    /// nothing, at load — it doesn't itself evaluate). Declared per variant;
    /// defaults to `scalar`.
    pub output: String,
    /// Field accessors for a `struct` output. Empty for fugazi today — its
    /// multi-output indicators are modelled as separate scalar tags
    /// (`macd_line`, `bb_upper`, …) rather than one struct-output tag.
    pub projections: Vec<String>,
    /// The fine **conceptual sub-group** — `moving averages`, `oscillators`,
    /// `bands`, `trend / directional`, … — one rung finer than `kind`, for
    /// consumers that present the vocabulary in curated sections (the CLI
    /// `list indicators` catalogue, editor autocomplete groups, doc headings).
    /// Editorial, so it can't be reflected off the type: it's stamped from the
    /// [`CATEGORIES`] taxonomy, which a test pins to cover every tag exactly
    /// once. Never empty in a `spec_grammar()` record.
    pub category: String,
    /// The variant's `///` doc, if any.
    pub doc: Option<String>,
    /// The release the tag first shipped in. [`SINCE_BASELINE`] for everything
    /// present at that baseline; a real version for anything added since.
    pub since: String,
    /// `true` when *resolving* this tag touches something outside the
    /// document itself — today, only `import` (a filesystem read). A purely
    /// descriptive fact, not an enforcement mechanism: fugazi has no
    /// deployment-policy concept, and nothing here changes what a caller can
    /// author. An embedder that hosts user-authored documents and wants to
    /// deny this class of tag (or generate matching editor tooling) can read
    /// this instead of hand-maintaining its own table pinned to fugazi's tag
    /// list. `false` for every other tag.
    pub host_affecting: bool,
}

impl GrammarTag {
    /// The canonical spelling — `forms[0]`, the one a generator should emit.
    ///
    /// Use this when you are *producing* a document. When you are *accepting*
    /// one (validating, completing, scaffolding), iterate [`forms`](Self::forms)
    /// instead: the canonical form is not the only one that parses.
    ///
    /// # Panics
    ///
    /// If `forms` is empty, which the derive and `document_grammar_tags` never
    /// produce and `tests/spec_grammar.rs` pins.
    pub fn canonical(&self) -> &GrammarForm {
        self.forms.first().expect("every tag has a canonical form")
    }
}

/// Every tag in every document vocabulary. The one authority `spec_tags()`, the
/// Python constructors, and downstream tooling all derive from.
///
/// Five `group`s, in two tiers:
///
/// - **Reflected off serde** via `#[derive(SpecGrammar)]`, so they cannot drift:
///   `node` (the composable expression enum), `selection` (a `basket:`
///   document's `selection:` rules), and `universe` (`!all_of`/`!any_of`).
/// - **Hand-authored** (`document_grammar_tags`) for the load-time tags that
///   are *not* serde variants — they're `Value` rewrites resolved before the
///   typed parse, so there is no variant for the derive to read. `weighting`
///   (`!fixed`/`!equal_weight`) and `document` (`!import`/`!param`/`!arg`/
///   `!undefined`). Their name set is pinned to the parser's own
///   [`REWRITTEN_TAGS`](crate::spec::typecheck::REWRITTEN_TAGS) (plus `fixed`) by
///   a test, the same anti-drift guarantee one rung lower.
///
/// The expression JSON schema ([`spec_json_schema`]) draws only on the `node`
/// and `selection` groups; the other three are document-level directives, not
/// slot-fillable expressions.
///
/// **Not** covered, by design: the nested config sub-documents
/// (`TradingCostsConfig`, a portfolio child's embedded strategy) — these are
/// whole documents, not slot-level tags. The Python `spec_grammar()` docstring
/// records this so the contract is explicit.
pub fn spec_grammar() -> Vec<GrammarTag> {
    let mut tags = crate::spec::expr::NodeSpec::grammar_tags();
    tags.extend(crate::spec::basket::SelectionRuleSpec::grammar_tags());
    tags.extend(crate::spec::basket::UniverseSpec::grammar_tags());
    tags.extend(document_grammar_tags());
    // Stamp the editorial `category` the derive left blank. `CATEGORIES` is the
    // one authority for the taxonomy (and its curated order); a test pins it to
    // cover every tag exactly once, so a missing stamp is a test failure, not a
    // silent empty string shipped to consumers.
    for tag in &mut tags {
        tag.category = category_of(&tag.name).to_owned();
        stamp_node_outputs(tag);
    }
    tags
}

/// Stamp the output demand on every expression slot of `tag`, read from
/// `check`'s own table via [`slot_demand`](crate::spec::typecheck::slot_demand).
///
/// Only the `node` group: the `selection` / `universe` / `weighting` /
/// `document` vocabularies have no expression slots the type checker rules on,
/// so their fields stay unstamped rather than being reported as unconstrained.
fn stamp_node_outputs(tag: &mut GrammarTag) {
    use crate::spec::typecheck::slot_demand;
    if tag.group != "node" {
        return;
    }
    // Every form, not just the canonical one: an alternate spelling holds the
    // *same* slots under different syntax, so a consumer completing inside
    // `!changed { source: ` needs the demand there too.
    for form in &mut tag.forms {
        for field in &mut form.fields {
            // `!match`'s `cases:` holds one expression per case, all under the
            // same demand; the table names that pseudo-slot `case value`.
            let slot = match field.ty.as_str() {
                "node" | "node_list" => field.name.as_str(),
                "match_cases" => "case value",
                _ => continue,
            };
            field.node_output = slot_demand(&tag.name, slot).map(demand_labels);
        }
        // A `newtype` / `seq` spelling has no named fields, so its payload's
        // demand is the tag's only slot — `source` for the unary wrappers,
        // `item` for the folds. Take whichever the table reports rather than
        // re-deriving it.
        if matches!(form.payload.as_deref(), Some("node" | "node_list")) {
            form.payload_output = crate::spec::typecheck::slot_demands(&tag.name)
                .into_iter()
                .next()
                .map(|(_, types)| demand_labels(types));
        }
    }
}

/// The YAML fragment a **defaulted slot's value** is written as — the
/// [`GrammarField::default_expr`] the derive reports for a field whose serde
/// default is a node rather than a JSON literal.
///
/// Read off the default's own `Debug`, the same reflection
/// [`tag_name`](crate::spec::typecheck::tag_name) uses to name a variant
/// without a parallel table: change what `default_source` returns and the
/// fragment changes with it. That is the whole point — the equivalence
/// "`!ema { period: 10 }` is `!ema { source: !close, period: 10 }`" is exact and
/// machine-checkable, and it should not have to be recovered by regexing English
/// out of a doc string.
///
/// Only the **bare spelling** is claimed, and only for a value sitting entirely
/// at its own defaults: `Close { source: None }` → `!close`, `Everything` →
/// `!everything`. That bare leaf is the *root floor* the descriptor reports —
/// it carries its own blessed-series root implicitly, so there is never a
/// deeper `{ source: … }` to spell, and a fragment is always one token. A
/// default carrying a real field value (`!value 0`, a list) has a fragment this
/// does not know how to spell, so it answers `None` — the descriptor reports
/// "no expressible default" rather than a wrong one. It does
/// not stay silent, though: `defaulted_expression_slots_name_their_default` in
/// `tests/spec_grammar.rs` fails on such a slot, because the fix is to teach
/// this function the spelling, not to leave the fact in prose.
///
/// Takes the default **by value** so a `Box<NodeSpec>` default fn's return can
/// be handed over directly (`Box` forwards `Debug` to its contents).
pub(crate) fn default_expr_of<T: std::fmt::Debug>(value: T) -> Option<String> {
    let debug = format!("{value:?}");
    let ident: String = debug
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect();
    if ident.is_empty() || !all_fields_defaulted(debug[ident.len()..].trim()) {
        return None;
    }
    Some(crate::spec::typecheck::snake_tag(&debug))
}

/// Whether a variant's `Debug` *body* shows every field at its own default —
/// `""` for a unit variant, `{ a: None, b: None }` for a struct one. Those are
/// exactly the values the bare `!tag` spelling parses back to.
///
/// Deliberately strict: a newtype payload (`(1)`), or any field holding a real
/// value, answers `false`. A false negative costs a `default_expr`; a false
/// positive would ship a fragment that means something else.
fn all_fields_defaulted(body: &str) -> bool {
    let Some(inner) = body.strip_prefix('{').and_then(|s| s.strip_suffix('}')) else {
        return body.is_empty();
    };
    inner.split(',').all(|field| match field.split_once(':') {
        Some((name, value)) => {
            let name = name.trim();
            !name.is_empty()
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && value.trim() == "None"
        }
        None => false,
    })
}

/// A runtime type spelled in the descriptor's own [`GrammarTag::output`]
/// vocabulary, so a slot's demand can be compared against a candidate tag's
/// `output` by string equality.
///
/// The descriptor calls a `Real` output `scalar`; the rest are `PayloadType`'s
/// own names, lowercased.
pub fn output_label(ty: crate::runtime::PayloadType) -> &'static str {
    use crate::runtime::PayloadType as P;
    match ty {
        P::Real => "scalar",
        P::Bool => "bool",
        P::Str => "str",
        P::Atom => "atom",
        P::Candle => "candle",
        P::Time => "time",
        P::Snapshot => "snapshot",
    }
}

/// [`output_label`] over a whole demand.
fn demand_labels(types: Vec<crate::runtime::PayloadType>) -> Vec<String> {
    types.into_iter().map(output_label).map(str::to_owned).collect()
}

/// The fine conceptual **sub-group** of every tag, in curated order — the
/// taxonomy one rung finer than [`GrammarTag::kind`]. The single source both the
/// descriptor's `category` field and the CLI `list indicators` catalogue draw
/// from; `list` renders sections in this order, and each tag within a section in
/// this order, so the curation lives here rather than being re-encoded per
/// consumer.
///
/// Pinned by `categories_cover_every_tag_once` to name exactly the
/// [`spec_grammar`] tag set, each once — a new tag fails the tests until it's
/// placed. Sections are alphabetical by label (a test checks it), so a new one
/// lands predictably. Public so consumers (the CLI catalogue, external tooling)
/// can render the curated grouping and order directly, not just the per-tag
/// `category` string.
pub const CATEGORIES: &[(&str, &[&str])] = &[
    ("arithmetic operators", &["add", "sub", "mul", "div", "log", "exp"]),
    (
        "bands",
        &[
            "bb_upper", "bb_middle", "bb_lower", "keltner_upper", "keltner_middle",
            "keltner_lower", "donchian_upper", "donchian_middle", "donchian_lower",
        ],
    ),
    (
        "bar indicators",
        &[
            "atr", "mfi", "vwap", "true_range", "obv", "ad", "parkinson", "garman_klass",
            "rogers_satchell",
        ],
    ),
    ("basket selection", &["everything", "top_bottom", "threshold", "quantile"]),
    ("basket universe", &["all_of", "any_of"]),
    ("boolean logic", &["and", "or", "xor", "all", "any", "not"]),
    (
        "calendar",
        &[
            "year", "month", "day", "hour", "minute", "second", "day_of_week", "day_of_year",
            "week_of_year", "quarter", "unix_seconds", "unix_millis", "time", "is_weekday",
            "is_weekend",
        ],
    ),
    (
        "candle leaves",
        &["close", "high", "low", "open", "volume", "typical", "median", "current", "pick"],
    ),
    ("comparisons", &["str_eq", "str_ne", "gt", "lt", "ge", "le", "eq", "ne"]),
    ("conditional", &["if_else", "match"]),
    ("constants", &["value", "never", "every"]),
    ("cross-timeframe composition", &["resample", "latch"]),
    ("crossovers", &["crosses_above", "crosses_below"]),
    ("edge detectors", &["changed", "became_true", "became_false"]),
    ("event timing", &["bars_since", "bars_since_high", "bars_since_low"]),
    ("level comparisons", &["above", "below"]),
    ("load-time placeholders", &["param", "arg", "import", "undefined"]),
    ("lookback operators", &["lag", "diff", "ratio", "roc"]),
    ("macd", &["macd_line", "macd_signal", "macd_histogram"]),
    ("moving averages", &["sma", "ema", "rma", "wma", "hma"]),
    ("oscillators", &["rsi", "stddev", "cci", "stochastic", "stoch_rsi", "williams_r"]),
    ("overlay side channel", &["get", "has_column"]),
    ("portfolio weighting", &["equal_weight", "fixed"]),
    ("position anchors", &["entry", "peak", "trough"]),
    ("rolling extrema", &["rolling_max", "rolling_min"]),
    (
        "rolling statistics",
        &[
            "correlation", "kurtosis", "percentile", "percentile_rank", "skewness",
            "variance_ratio", "zscore",
        ],
    ),
    (
        "sizing helpers",
        &["vol_target", "atr_risk", "drawdown_throttle", "equity_vol_target", "fractional_kelly"],
    ),
    (
        "strategy book",
        &[
            "equity", "equity_peak", "drawdown", "return_per_bar", "trade_pnl", "trade_return",
            "strategy_book", "portfolio_book",
        ],
    ),
    ("trailing strategy risk", &["sharpe", "sortino", "volatility", "max_drawdown", "calmar"]),
    (
        "trend / directional",
        &[
            "adx", "plus_di", "minus_di", "dmi_plus_di", "dmi_minus_di", "aroon_up", "aroon_down",
            "aroon_oscillator", "sar",
        ],
    ),
    ("unstable pass-through", &["unstable"]),
];

/// The category a tag belongs to, from [`CATEGORIES`]. `""` if unclassified —
/// which the taxonomy test forbids, so a `spec_grammar()` record never carries
/// an empty one.
fn category_of(name: &str) -> &'static str {
    for (label, tags) in CATEGORIES {
        if tags.contains(&name) {
            return label;
        }
    }
    ""
}

/// The hand-authored records for the load-time rewrite tags — the one place in
/// the descriptor that is *not* reflected off a serde variant, because these
/// tags never reach the typed parse: a `Value` pass rewrites each away first
/// (`!fixed`/`!equal_weight` → `!value`, `!import`/`!param`/`!arg` → their
/// resolved subtree, `!undefined` a check-mode stand-in).
///
/// Their `name` set is pinned to [`REWRITTEN_TAGS`](crate::spec::typecheck::REWRITTEN_TAGS)
/// (plus `fixed`, which the portfolio `weights:` sugar recognises but which
/// isn't a placeholder) by `tests/spec_grammar.rs`, so a new load-time tag can't
/// ship without a row here. `output` is `none`: none of them evaluate to a
/// runtime value — they resolve to another node (or nothing) at load.
fn document_grammar_tags() -> Vec<GrammarTag> {
    /// A `newtype` form carrying one positional value of grammar type `payload`.
    fn newtype(payload: &str, scope: Option<&str>, doc: Option<&str>) -> GrammarForm {
        GrammarForm {
            shape: "newtype".to_owned(),
            fields: Vec::new(),
            payload: Some(payload.to_owned()),
            payload_output: None,
            scope: scope.map(str::to_owned),
            doc: doc.map(str::to_owned),
        }
    }
    /// A `map` form over the given `(name, type, required, doc)` keys.
    fn map(
        fields: &[(&str, &str, bool, &str)],
        scope: Option<&str>,
        doc: Option<&str>,
    ) -> GrammarForm {
        GrammarForm {
            shape: "map".to_owned(),
            fields: fields
                .iter()
                .map(|(name, ty, required, doc)| GrammarField {
                    name: (*name).to_owned(),
                    ty: (*ty).to_owned(),
                    required: *required,
                    // None of the load-time tags defaults a key at all:
                    // `!param`/`!arg`'s `default:` is an arbitrary value tree
                    // the *author* supplies, and `!import`'s `params:` is a
                    // table, not an expression.
                    default: None,
                    node_output: None,
                    doc: Some((*doc).to_owned()),
                })
                .collect(),
            payload: None,
            payload_output: None,
            scope: scope.map(str::to_owned),
            doc: doc.map(str::to_owned),
        }
    }
    fn tag(
        name: &str,
        group: &str,
        kind: &str,
        forms: Vec<GrammarForm>,
        doc: &str,
    ) -> GrammarTag {
        GrammarTag {
            name: name.to_owned(),
            group: group.to_owned(),
            kind: kind.to_owned(),
            forms,
            output: "none".to_owned(),
            projections: Vec::new(),
            // Stamped by `spec_grammar` from `CATEGORIES`, like every other tag.
            category: String::new(),
            doc: Some(doc.to_owned()),
            since: SINCE_BASELINE.to_owned(),
            // Only `import` reads the filesystem; every other hand-authored
            // row is a pure `Value`-tree rewrite. Flipped below for `import`.
            host_affecting: false,
        }
    }
    // The `{ key, default }` body `!param` and `!arg` share verbatim —
    // `spec/args.rs` says so in as many words ("the `!arg` grammar mirrors
    // `!param`"), and both are read by the same two-arm match on a string or an
    // object with a string `key`.
    const PLACEHOLDER_BODY: &[(&str, &str, bool, &str)] = &[
        (
            "key",
            "str",
            true,
            "The placeholder's name — what a `--params NAME=…` term (or the \
             driver's per-symbol binding) is matched against.",
        ),
        (
            "default",
            "other",
            false,
            "The value to fall back to when the name is unset. Any value tree, \
             not just a scalar. Omitting it makes the placeholder **required**: \
             an unset one is an error at load (`fugazi check` holds it as a \
             typed hole instead).",
        ),
    ];
    vec![
        // --- weighting: portfolio `weights:` sugar --------------------------
        tag(
            "fixed", "weighting", "weighting",
            vec![GrammarForm {
                shape: "seq".to_owned(),
                fields: Vec::new(),
                payload: Some("number_list".to_owned()),
                payload_output: None,
                scope: Some("portfolio_weights".to_owned()),
                doc: None,
            }],
            "Portfolio `weights:` sugar. `!fixed [w0, w1, …]` assigns a literal \
             weight per child by position; rewritten to `!value [w0, w1, …]` \
             (per-child indexed) at load.",
        ),
        tag(
            "equal_weight", "weighting", "weighting",
            vec![
                GrammarForm {
                    shape: "unit".to_owned(),
                    fields: Vec::new(),
                    payload: None,
                    payload_output: None,
                    scope: Some("portfolio_weights".to_owned()),
                    doc: None,
                },
                newtype(
                    "positive_uint",
                    None,
                    Some(
                        "The sizing spelling, and a different tag entirely in \
                         meaning: `!equal_weight <N>` lowers to `!value <1/N>`, a \
                         constant fraction for a known leg count, and is accepted \
                         wherever a node is. The bare form lowers to `!value 1.0` \
                         and only means anything in a portfolio `weights:` \
                         template, where every child's 1.0 normalises to 1/N at \
                         rebalance.",
                    ),
                ),
            ],
            "Equal-weight sugar. Bare `!equal_weight` in a portfolio `weights:` \
             template lowers to `!value 1.0` (each child normalises to 1/N at \
             rebalance); as sizing, `!equal_weight <N>` lowers to `!value <1/N>`. \
             Resolved before the typed parse.",
        ),
        // --- document: load-time composition / substitution -----------------
        GrammarTag {
            host_affecting: true,
            ..tag(
                "import", "document", "document",
                vec![
                    newtype("str", None, None),
                    map(
                        &[
                            (
                                "path",
                                "str",
                                true,
                                "The document to splice in, resolved against the \
                                 importing document's own directory.",
                            ),
                            (
                                "params",
                                "other",
                                false,
                                "A `NAME: value` mapping the imported subtree's own \
                                 `!param` placeholders resolve against **first**. A \
                                 key not listed here falls through to the outer \
                                 document's `--params` pass, so one fragment can be \
                                 imported N times with N parameterizations.",
                            ),
                        ],
                        None,
                        Some(
                            "The only spelling that can carry inline `params:` — the \
                             shape a portfolio-of-strategies document needs, where the \
                             same fragment is imported once per child with different \
                             values.",
                        ),
                    ),
                ],
                "Document composition. `!import <path>` splices another YAML spec at \
                 load time (the `!import { path, params }` form passes inline params); \
                 resolved by `imports::resolve` before parse, confined to the loader's \
                 `base_dir` — or refused outright by a caller that disables imports.",
            )
        },
        tag(
            "param", "document", "document",
            vec![
                newtype("str", None, None),
                map(
                    PLACEHOLDER_BODY,
                    None,
                    Some(
                        "The only spelling that can carry a `default:`. The bare \
                         form is exactly `{ key: NAME }` — always required, never \
                         defaulted.",
                    ),
                ),
            ],
            "Load-time substitution placeholder, legal in any value position — an \
             expression slot, a scalar field like `period:`, a string field like \
             `symbol:`, a list element. Replaced from the `--params` / `params:` \
             table by `params::substitute` before the typed parse.",
        ),
        tag(
            "arg", "document", "document",
            vec![
                newtype("str", Some("template"), None),
                map(
                    PLACEHOLDER_BODY,
                    Some("template"),
                    Some(
                        "The only spelling that can carry a `default:` — what a \
                         template falls back to when the driver doesn't bind that \
                         name.",
                    ),
                ),
            ],
            "Build-time substitution placeholder. `!arg SYM` / `!arg CHILD_NAME` / \
             `!arg CHILD_INDEX` is substituted per symbol or child by \
             `args::substitute` when a per-leg template is built. Unlike `!param` \
             it is **not** legal everywhere: nothing substitutes it outside a \
             deferred template body, so one written elsewhere is a parse error.",
        ),
        tag(
            "undefined", "document", "document",
            vec![GrammarForm {
                shape: "unit".to_owned(),
                fields: Vec::new(),
                payload: None,
                payload_output: None,
                scope: Some("internal".to_owned()),
                doc: None,
            }],
            "Internal check-mode stand-in for a not-yet-substituted `!arg` / \
             `!param`, letting a `SpecTemplate` type-check with its placeholders \
             held undefined. Never authored by hand — a document still carrying \
             one is refused at run.",
        ),
    ]
}

/// [`spec_grammar`] wrapped with its [`SCHEMA_VERSION`] as the top-level
/// document the Python `spec_grammar()` returns: `{ schema_version, tags }`.
pub fn spec_grammar_document() -> serde_json::Value {
    serde_json::json!({
        "schema_version": SCHEMA_VERSION,
        "tags": spec_grammar(),
    })
}

/// A JSON Schema (draft 2020-12) for the spec's **expression grammar**, a second
/// projection of [`spec_grammar`].
///
/// Validates the **JSON bridge encoding** of an expression — the single-key
/// `{ "<tag>": { <fields> } }` form that `NodeSpec`'s `TryFrom` normalises to
/// and that the Python dict path / web form produce — plus the bare-literal
/// shorthands (`70` → `!value`, `"close"` → `!close`) and the load-time
/// placeholders an *authored* spec carries (`!arg` / `!param` / `!import` /
/// `!undefined` / `!equal_weight`, from `typecheck::REWRITTEN_TAGS`), which the
/// build resolves before the typed parse. `additionalProperties: false` mirrors
/// `deny_unknown_fields`. Structure only — the Real/Bool/Str type discipline
/// stays in `typecheck.rs`, so this complements `fugazi check`, not replaces it.
///
/// The root `$ref`s `#/$defs/node`; `#/$defs/selection` is exposed for the
/// `basket:` `selection:` vocabulary. See [`spec_document_json_schema`] for the
/// whole-document envelope.
pub fn spec_json_schema() -> serde_json::Value {
    serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": format!("https://fugazi.dev/spec/{}/node.schema.json", env!("CARGO_PKG_VERSION")),
        "$comment": "Validates the JSON bridge form of a fugazi expression \
                     (single-key {tag: body} objects + bare-literal shorthands + \
                     authored load-time placeholders), not the YAML !tag surface. \
                     Structure only; the type discipline stays in typecheck.rs.",
        "title": "fugazi spec expression",
        "$ref": "#/$defs/node",
        "$defs": expression_defs(),
    })
}

/// A JSON Schema (draft 2020-12) for a whole **spec document** — the five
/// strategy shapes (single / pairs / basket / multi / portfolio) and their
/// slots, `$ref`-ing the same `node` / `selection` grammar for every expression.
/// Phase 2 of the proposal.
///
/// The root is a `oneOf` over the five shapes, which are disjoint by their
/// required keys (only `multi` has none — it's the structural fallback). Same
/// caveats as [`spec_json_schema`]: it validates the JSON bridge form and checks
/// *structure*; it is complementary to `fugazi check`, not a replacement.
/// Nested portfolio-child strategies are validated only as non-empty objects
/// (presets + structural shape-detection are out of scope this iteration).
pub fn spec_document_json_schema() -> serde_json::Value {
    let mut defs = expression_defs();
    defs.insert("side".into(), side_def());
    defs.insert("basket_side".into(), basket_side_def());
    defs.insert("multi_side".into(), side_def());
    defs.insert("universe".into(), universe_def());
    defs.insert("portfolio_child".into(), portfolio_child_def());
    defs.insert("single".into(), doc_single());
    defs.insert("pairs".into(), doc_pairs());
    defs.insert("basket".into(), doc_basket());
    defs.insert("multi".into(), doc_multi());
    defs.insert("portfolio".into(), doc_portfolio());

    serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": format!("https://fugazi.dev/spec/{}/document.schema.json", env!("CARGO_PKG_VERSION")),
        "$comment": "Validates the JSON bridge form of a whole fugazi strategy \
                     document. Shape is a oneOf over single/pairs/basket/multi/\
                     portfolio (disjoint by required keys). Structure only.",
        "title": "fugazi spec document",
        "oneOf": [
            def_ref("single"), def_ref("pairs"), def_ref("basket"),
            def_ref("multi"), def_ref("portfolio"),
        ],
        "$defs": defs,
    })
}

/// The `$defs` shared by the expression and document schemas: the recursive
/// `node` / `selection` vocabularies and their leaves.
fn expression_defs() -> serde_json::Map<String, serde_json::Value> {
    use serde_json::{Value, json};

    let mut node_variants: Vec<Value> = Vec::new();
    let mut selection_variants: Vec<Value> = Vec::new();
    for tag in spec_grammar() {
        // Only the two *expression* vocabularies belong in this schema; the
        // `universe` / `weighting` / `document` groups are document-level
        // directives, not slot-fillable nodes.
        match tag.group.as_str() {
            "node" => node_variants.push(tag_schema(&tag)),
            "selection" => selection_variants.push(tag_schema(&tag)),
            _ => {}
        }
    }
    // Bare-literal shorthands accepted anywhere a node is (normalised to `!value`).
    node_variants.push(json!({ "type": "number" }));
    node_variants.push(json!({ "type": "boolean" }));
    node_variants.push(json!({ "type": "array", "items": { "type": "number" } }));
    // Load-time placeholders / sugar valid in an *authored* spec, sourced from
    // the parser's own list so a new one flows in with no edit here.
    for &t in crate::spec::typecheck::REWRITTEN_TAGS {
        node_variants.push(json!({ "type": "string", "const": t }));
        node_variants.push(single_key(t, json!(true)));
    }

    // A load-time placeholder in *any* position, including a scalar field like
    // `!pick { symbol: !arg SYM }`: a single-key object whose key is a rewritten
    // tag. Scalar field schemas accept it (via `or_placeholder`) so authored
    // per-symbol templates validate; the build substitutes it before typing.
    let placeholder_keys: Vec<Value> =
        crate::spec::typecheck::REWRITTEN_TAGS.iter().map(|t| json!(t)).collect();

    let mut defs = serde_json::Map::new();
    defs.insert("node".into(), json!({ "oneOf": node_variants }));
    defs.insert("selection".into(), json!({ "oneOf": selection_variants }));
    defs.insert(
        "placeholder".into(),
        json!({
            "type": "object",
            "minProperties": 1,
            "maxProperties": 1,
            "propertyNames": { "enum": placeholder_keys },
        }),
    );
    defs.insert(
        "match_case".into(),
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["when", "value"],
            "properties": {
                "when": { "oneOf": [{ "type": "number" }, { "type": "string" }] },
                "value": node_ref(),
            },
        }),
    );
    // An embedded single-asset strategy document; opaque here.
    defs.insert("strategy".into(), json!(true));
    defs
}

fn node_ref() -> serde_json::Value {
    def_ref("node")
}

fn def_ref(name: &str) -> serde_json::Value {
    serde_json::json!({ "$ref": format!("#/$defs/{name}") })
}

/// `{ type:object, additionalProperties:false, properties, required }` from a
/// `(name, schema, required)` list.
fn object(props: &[(&str, serde_json::Value, bool)]) -> serde_json::Value {
    let mut properties = serde_json::Map::new();
    let mut required: Vec<serde_json::Value> = Vec::new();
    for (name, schema, req) in props {
        properties.insert((*name).to_owned(), schema.clone());
        if *req {
            required.push(serde_json::Value::from(*name));
        }
    }
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "properties": properties,
        "required": required,
    })
}

// --- document-envelope defs (the five strategy shapes + their slots) --------

/// `SideSpec` / `MultiSideSpec`: `enter` (required) + optional `exit` /
/// `stop_loss` / `take_profit`. Identical shape; `MultiSideSpec`'s slots are
/// per-symbol templates, which validate against the same `node`.
fn side_def() -> serde_json::Value {
    object(&[
        ("enter", node_ref(), true),
        ("exit", node_ref(), false),
        ("stop_loss", node_ref(), false),
        ("take_profit", node_ref(), false),
    ])
}

/// `BasketSideSpec`: optional per-symbol `stop_loss` / `take_profit` templates.
fn basket_side_def() -> serde_json::Value {
    object(&[
        ("stop_loss", node_ref(), false),
        ("take_profit", node_ref(), false),
    ])
}

/// `UniverseSpec`: `!all_of [ SYM… ]` / `!any_of [ SYM… ]` (absent ⇒ floating).
fn universe_def() -> serde_json::Value {
    let list = serde_json::json!({ "type": "array", "items": { "type": "string" } });
    serde_json::json!({
        "oneOf": [single_key("all_of", list.clone()), single_key("any_of", list)]
    })
}

/// `PortfolioChildSpec`: optional `name` / `group` + a required `strategy`.
/// The child strategy is validated only as a non-empty mapping (see the fn doc
/// on [`spec_document_json_schema`]).
fn portfolio_child_def() -> serde_json::Value {
    object(&[
        ("name", serde_json::json!({ "type": "string" }), false),
        ("group", serde_json::json!({ "type": "string" }), false),
        (
            "strategy",
            serde_json::json!({ "type": "object", "minProperties": 1 }),
            true,
        ),
        ("meta", meta_ref(), false),
    ])
}

/// The `meta:` slot every document shape carries: any value, uninterpreted.
/// `true` is JSON Schema's always-valid schema — exactly the open contract
/// [`spec::meta`](crate::spec::meta) promises, so a service's payload can never
/// fail document validation.
fn meta_ref() -> serde_json::Value {
    serde_json::json!({
        "$comment": "Free-form metadata; fugazi never interprets it.",
    })
}

fn doc_single() -> serde_json::Value {
    object(&[
        ("symbol", serde_json::json!({ "type": "string" }), true),
        ("long", def_ref("side"), false),
        ("short", def_ref("side"), false),
        ("sizing", node_ref(), false),
        ("rebalance_on", node_ref(), false),
        ("meta", meta_ref(), false),
    ])
}

fn doc_pairs() -> serde_json::Value {
    object(&[
        ("left", serde_json::json!({ "type": "string" }), true),
        ("right", serde_json::json!({ "type": "string" }), true),
        ("enter", node_ref(), false),
        ("exit", node_ref(), false),
        ("stop_loss", node_ref(), false),
        ("take_profit", node_ref(), false),
        ("long_spread", def_ref("side"), false),
        ("short_spread", def_ref("side"), false),
        ("sizing", node_ref(), false),
        ("rebalance_on", node_ref(), false),
        ("meta", meta_ref(), false),
    ])
}

fn doc_basket() -> serde_json::Value {
    object(&[
        ("selection", def_ref("selection"), true),
        ("score", node_ref(), true),
        ("sizing", node_ref(), true),
        ("universe", def_ref("universe"), false),
        ("rebalance_on", node_ref(), false),
        ("balance_sides", serde_json::json!({ "type": "boolean" }), false),
        ("long", def_ref("basket_side"), false),
        ("short", def_ref("basket_side"), false),
        ("meta", meta_ref(), false),
    ])
}

fn doc_multi() -> serde_json::Value {
    object(&[
        ("long", def_ref("multi_side"), false),
        ("short", def_ref("multi_side"), false),
        ("sizing", node_ref(), false),
        ("universe", def_ref("universe"), false),
        ("rebalance_on", node_ref(), false),
        ("meta", meta_ref(), false),
    ])
}

fn doc_portfolio() -> serde_json::Value {
    object(&[
        (
            "children",
            serde_json::json!({ "type": "array", "items": def_ref("portfolio_child"), "minItems": 1 }),
            true,
        ),
        ("weights", node_ref(), false),
        ("rebalance_on", node_ref(), false),
        ("rebalance_policy", serde_json::json!(true), false),
        ("meta", meta_ref(), false),
    ])
}

/// The schema for one tag, keyed by its `shape`.
///
/// Any tag with **no required fields** may be written bare — `close` / `!close`
/// normalise to `{close:{}}`, so both forms are accepted. That covers unit tags
/// and every all-optional map tag (the atom leaves, calendar accessors, …).
fn tag_schema(tag: &GrammarTag) -> serde_json::Value {
    let mut forms = tag.forms.iter().map(|f| form_schema(&tag.name, f));
    let first = forms.next().expect("every tag has a canonical form");
    let rest: Vec<serde_json::Value> = forms.collect();
    if rest.is_empty() {
        return first;
    }
    // `anyOf`, not `oneOf`: these are alternative *spellings* of one tag, and
    // the question is whether the instance is written as any of them. `oneOf`
    // would additionally demand that no two forms ever match the same document,
    // which is a property nothing here needs and a future form could break.
    //
    // This union is why the schema now accepts `{"unstable": "close"}` and
    // `{"changed": {"source": …}}`, both of which the parser has always taken
    // and the single-`shape` schema rejected.
    let mut all = vec![first];
    all.extend(rest);
    serde_json::json!({ "anyOf": all })
}

/// The schema for one [`GrammarForm`] of a tag.
fn form_schema(name: &str, form: &GrammarForm) -> serde_json::Value {
    use serde_json::json;
    let bare = json!({ "type": "string", "const": name });
    match form.shape.as_str() {
        "map" => {
            if form.fields.iter().any(|f| f.required) {
                return single_key(name, map_body(&form.fields));
            }
            // No required key, so the body may be omitted entirely — as the bare
            // string, or as an explicit null, which is what a YAML `!close`
            // normalises to and what the parser has always taken. The null arm
            // used to be missing here, so the schema rejected a document fugazi
            // accepts (and, once fields carried defaults, its own advertised
            // `default`). The `unit` arm below has always had it.
            let body = json!({ "oneOf": [{ "type": "null" }, map_body(&form.fields)] });
            json!({ "oneOf": [bare, single_key(name, body)] })
        }
        "unit" => json!({
            "oneOf": [
                bare,
                single_key(name, json!({
                    "oneOf": [{ "type": "null" }, { "type": "object", "additionalProperties": false }]
                })),
            ]
        }),
        "newtype" => single_key(name, payload_schema(form.payload.as_deref())),
        "seq" => single_key(name, json!({ "type": "array", "items": node_ref() })),
        // Unreachable: every form is unit/map/newtype/seq. Stay permissive.
        _ => json!(true),
    }
}

/// `{ "type":"object", required:[name], additionalProperties:false, properties:{name: body} }`
fn single_key(name: &str, body: serde_json::Value) -> serde_json::Value {
    let mut props = serde_json::Map::new();
    props.insert(name.to_owned(), body);
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": [name],
        "properties": props,
    })
}

/// The `{ <fields> }` object body of a `map` tag.
fn map_body(fields: &[GrammarField]) -> serde_json::Value {
    let mut props = serde_json::Map::new();
    let mut required: Vec<serde_json::Value> = Vec::new();
    for f in fields {
        props.insert(f.name.clone(), field_schema(f));
        if f.required {
            required.push(serde_json::Value::from(f.name.clone()));
        }
    }
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "properties": props,
        "required": required,
    })
}

/// The schema for one named field, with its `description` / `default` attached.
///
/// JSON Schema's `default` is an *instance*, so each
/// [`GrammarDefault`] case is rendered in this schema's own encoding: a literal
/// goes in as-is, and a YAML fragment is normalised through the loader's own
/// YAML → JSON-bridge pass (`!close` → `"close"`), which is exactly the form
/// this schema validates. A fragment that somehow doesn't parse is dropped
/// rather than guessed at; `test_node_slot_defaults_validate` (the Python
/// suite, where a real JSON Schema engine lives) pins that it never comes to
/// that — every advertised default validates against the slot it sits on.
fn field_schema(field: &GrammarField) -> serde_json::Value {
    let mut schema = type_fragment(&field.ty);
    if let serde_json::Value::Object(map) = &mut schema {
        if let Some(doc) = &field.doc {
            map.insert("description".to_owned(), serde_json::Value::from(doc.clone()));
        }
        let default = match &field.default {
            Some(GrammarDefault::Literal(v)) => Some(v.clone()),
            Some(GrammarDefault::Expr(e)) => bridge_instance(e),
            None => None,
        };
        if let Some(default) = default {
            map.insert("default".to_owned(), default);
        }
    }
    schema
}

/// A [`GrammarDefault::Expr`] fragment in **this schema's** encoding: the JSON
/// bridge form, via the loader's own YAML → JSON pass.
///
/// `!close` parses to `{"close": null}` — a tagged empty scalar. That is a legal
/// document (the parser takes it, and so does this schema), but the *canonical*
/// bridge spelling of a parameterless leaf is the bare string `"close"`, which
/// is what `forms[0]` describes and what a generator should emit. Advertise
/// that, so a consumer inserting the schema's `default` writes the same thing a
/// consumer inserting the descriptor's fragment does.
///
/// `None` if the fragment doesn't parse, which would be a broken invariant —
/// the Python suite's `test_node_slot_defaults_validate` fails rather than
/// letting a slot quietly lose its default.
fn bridge_instance(fragment: &str) -> Option<serde_json::Value> {
    let value = crate::spec::input::parse_value(fragment).ok()?;
    // `{"close": null}` — a lone tag with an empty body — is the bare `"close"`.
    let empty_tag = value.as_object().and_then(|map| match map.iter().next() {
        Some((tag, serde_json::Value::Null)) if map.len() == 1 => Some(tag.clone()),
        _ => None,
    });
    Some(empty_tag.map_or(value, serde_json::Value::from))
}

/// The bare positional payload of a `newtype` tag, from its grammar `payload` type.
fn payload_schema(payload: Option<&str>) -> serde_json::Value {
    match payload {
        Some(ty) => type_fragment(ty),
        None => serde_json::json!(true),
    }
}

/// A scalar leaf may instead be a load-time placeholder in an authored template
/// (`period: !arg P`), so every scalar fragment admits `#/$defs/placeholder`.
fn or_placeholder(base: serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "anyOf": [base, { "$ref": "#/$defs/placeholder" }] })
}

/// The closed grammar-`type` → JSON-Schema-fragment map.
fn type_fragment(ty: &str) -> serde_json::Value {
    use serde_json::json;
    match ty {
        "node" => node_ref(),
        "node_list" => json!({ "type": "array", "items": node_ref() }),
        // List leaves carried by document-level tags (universe / weighting
        // sugar). Present for completeness; those tags are excluded from the
        // expression schema, so these arms aren't reached through `node`.
        "str_list" => json!({ "type": "array", "items": { "type": "string" } }),
        "number_list" => json!({ "type": "array", "items": { "type": "number" } }),
        // A plain count that may legitimately be 0 (`longs` / `shorts` on a
        // selection rule).
        "uint" => or_placeholder(json!({ "type": "integer", "minimum": 0 })),
        // A period or window length. The spec field is a `NonZeroUsize`, so
        // serde rejects 0 at parse time and the schema can say so — this used
        // to advertise `minimum: 0` while the constructor asserted `> 0`.
        "positive_uint" => or_placeholder(json!({ "type": "integer", "minimum": 1 })),
        "number" => or_placeholder(json!({ "type": "number" })),
        "str" => or_placeholder(json!({ "type": "string" })),
        "bool" => or_placeholder(json!({ "type": "boolean" })),
        "str_operand" => json!({ "oneOf": [{ "type": "string" }, node_ref()] }),
        "match_cases" => json!({ "type": "array", "items": { "$ref": "#/$defs/match_case" } }),
        "strategy" => json!({ "$ref": "#/$defs/strategy" }),
        "selection" => json!({ "$ref": "#/$defs/selection" }),
        "literal" => json!({
            "oneOf": [
                { "type": "number" },
                { "type": "string" },
                { "type": "boolean" },
                { "type": "array", "items": { "type": "number" } },
            ]
        }),
        // `other` means a field type the mapper doesn't model yet — permissive,
        // never wrong. Driven toward zero by the grammar test.
        _ => json!(true),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{CATEGORIES, spec_grammar};

    /// The taxonomy must name exactly the descriptor's tag set — each tag once,
    /// no unknown names. This is what lets `category` be a total, drift-proof
    /// function of the tag: a new tag can't ship uncategorised (it fails here),
    /// and a renamed/removed tag can't linger in the table.
    #[test]
    fn categories_cover_every_tag_once() {
        let mut flat: Vec<&str> = Vec::new();
        for (_, tags) in CATEGORIES {
            flat.extend(*tags);
        }
        let unique: BTreeSet<&str> = flat.iter().copied().collect();
        assert_eq!(
            flat.len(),
            unique.len(),
            "a tag is listed under two categories: {:?}",
            {
                let mut seen = BTreeSet::new();
                flat.iter().filter(|t| !seen.insert(**t)).collect::<Vec<_>>()
            }
        );

        let want: BTreeSet<String> = spec_grammar().iter().map(|t| t.name.clone()).collect();
        let got: BTreeSet<String> = unique.iter().map(|s| s.to_string()).collect();
        let missing: Vec<_> = want.difference(&got).collect();
        let extra: Vec<_> = got.difference(&want).collect();
        assert!(missing.is_empty(), "tags with no category — add each to CATEGORIES: {missing:?}");
        assert!(extra.is_empty(), "CATEGORIES names tags the grammar doesn't have: {extra:?}");
    }

    /// Sections render in table order; keeping the labels alphabetical means a
    /// new one lands predictably rather than wherever it was appended.
    #[test]
    fn categories_are_alphabetical() {
        let labels: Vec<String> = CATEGORIES.iter().map(|(l, _)| l.to_lowercase()).collect();
        let mut sorted = labels.clone();
        sorted.sort();
        assert_eq!(labels, sorted, "CATEGORIES is not in alphabetical order of label");
    }

    /// Every stamped record carries its category — the contract consumers rely on.
    #[test]
    fn every_record_is_categorised() {
        let blank: Vec<String> = spec_grammar()
            .into_iter()
            .filter(|t| t.category.trim().is_empty())
            .map(|t| t.name)
            .collect();
        assert!(blank.is_empty(), "records with an empty category: {blank:?}");
    }
}
