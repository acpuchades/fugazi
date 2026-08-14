//! The machine-readable **grammar descriptor** — one JSON-serializable record
//! per YAML tag, reflected straight off the serde variant definitions.
//!
//! [`spec_tags`](crate::spec::typecheck::known_node_tags) already reads the tag
//! *names* off serde's own variant list so it cannot go stale. This module
//! carries the same anti-drift guarantee one level deeper: names, shapes,
//! fields, defaults, and prose all flow from the `#[derive(SpecGrammar)]` on
//! [`NodeSpec`](crate::spec::NodeSpec) and
//! [`SelectionRuleSpec`](crate::spec::basket::SelectionRuleSpec), so downstream
//! consumers (fugazi's own Python constructors, editor tooling, docs, the web
//! service's grammar table) generate from one authority instead of
//! re-encoding each tag by hand.
//!
//! The derive fills every field except the three that serde cannot know —
//! `kind`, `output`, and `since` — which are declared next to each variant via
//! `#[grammar(kind = "…", output = "…", since = "…")]` (see the derive in the
//! `fugazi-derive` crate). `kind` is mandatory: a new variant fails to compile
//! until it is classified, the same "every tag is a decision" discipline the
//! Python parity test enforces.

use serde::Serialize;

/// Bumped when the *shape* of the descriptor changes (a field added/removed/
/// renamed, or a legend value's meaning changes) so downstream consumers can
/// guard. Adding a *tag* does not bump this; renaming a tag's `name` is a
/// breaking change tracked by `since` instead.
pub const SCHEMA_VERSION: u32 = 1;

/// The `since` stamped on every tag that shipped at or before this release —
/// the baseline. Tags added afterwards carry their own real `since` via
/// `#[grammar(since = "X.Y")]`. See the module docs.
pub const SINCE_BASELINE: &str = "0.46";

/// One field of a `map`-shaped tag (the YAML body's keys). Empty for
/// `unit` / `newtype` / `seq` tags, which have no named fields.
#[derive(Debug, Clone, Serialize)]
pub struct GrammarField {
    /// The YAML key, exactly as written under the tag.
    pub name: String,
    /// The value's grammar type: `node` (a nested expression) · `node_list` ·
    /// `uint` · `number` · `str` · `bool` · `strategy` (an embedded strategy
    /// document) · `match_cases` · `str_operand`. A closed vocabulary the
    /// derive maps each Rust field type onto.
    #[serde(rename = "type")]
    pub ty: String,
    /// `false` when the key may be omitted — either the Rust field is an
    /// `Option`, or it carries a serde `default`.
    pub required: bool,
    /// The default value when omitted, for scalar fields with a serde
    /// `default` (e.g. `macd`'s `fast: 12`). `null` for required fields and
    /// for node-typed fields (whose default is a blessed-series fallback, not
    /// a literal).
    pub default: Option<serde_json::Value>,
    /// The field's `///` doc, if any.
    pub doc: Option<String>,
}

/// One tag's full grammar record. See the module docs for the legend.
#[derive(Debug, Clone, Serialize)]
pub struct GrammarTag {
    /// Variant name, without the leading `!`. A public contract — downstream
    /// codegens off it and anchors docs at `#tag-<name>`. Identical to the
    /// corresponding entry of `spec_tags()`.
    pub name: String,
    /// `node` (the composable expression vocabulary) or `selection` (a
    /// `basket:` document's `selection:` rules).
    pub group: String,
    /// `source` · `indicator` · `operator` · `predicate` · `function` ·
    /// `selection`. The semantic family; declared per variant.
    pub kind: String,
    /// How the tag is written in YAML: `unit` (bare `!foo`) · `newtype`
    /// (`!foo <x>`) · `seq` (`!foo [ … ]`) · `map` (`!foo { … }`).
    pub shape: String,
    /// The `map` body's keys; empty for the other shapes.
    pub fields: Vec<GrammarField>,
    /// What the tag evaluates to: `scalar` (a `Real`) · `bool` · `str` ·
    /// `time` · `candle` · `atom` · `book` · `any` (schema- or
    /// operand-dependent) · `selection` (a `selection`-group rule) · `struct`.
    /// Declared per variant; defaults to `scalar`.
    pub output: String,
    /// Field accessors for a `struct` output. Empty for fugazi today — its
    /// multi-output indicators are modelled as separate scalar tags
    /// (`macd_line`, `bb_upper`, …) rather than one struct-output tag.
    pub projections: Vec<String>,
    /// The variant's `///` doc, if any.
    pub doc: Option<String>,
    /// The release the tag first shipped in. [`SINCE_BASELINE`] for everything
    /// present at that baseline; a real version for anything added since.
    pub since: String,
}

/// Every tag in both vocabularies, reflected off the serde definitions. The one
/// authority `spec_tags()`, the Python constructors, and downstream tooling all
/// derive from.
pub fn spec_grammar() -> Vec<GrammarTag> {
    let mut tags = crate::spec::expr::NodeSpec::grammar_tags();
    tags.extend(crate::spec::basket::SelectionRuleSpec::grammar_tags());
    tags
}

/// [`spec_grammar`] wrapped with its [`SCHEMA_VERSION`] as the top-level
/// document the Python `spec_grammar()` returns: `{ schema_version, tags }`.
pub fn spec_grammar_document() -> serde_json::Value {
    serde_json::json!({
        "schema_version": SCHEMA_VERSION,
        "tags": spec_grammar(),
    })
}
