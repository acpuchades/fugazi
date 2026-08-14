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
///
/// - v1 (0.47): initial descriptor.
/// - v2 (0.48): added [`GrammarTag::payload`]; `literal` joined the field-type
///   vocabulary; `spec_json_schema()` shipped.
pub const SCHEMA_VERSION: u32 = 2;

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
    /// The grammar type of the single positional value a `newtype` / `seq` tag
    /// carries (which has no named `fields`): `node` for `!not <node>`, `uint`
    /// for `!every <n>`, `node_list` for `!all [ … ]`, `literal` for
    /// `!value <x>`. `None` for `unit` / `map` tags.
    pub payload: Option<String>,
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

/// A JSON Schema (draft 2020-12) for the spec's **expression grammar**, a second
/// projection of [`spec_grammar`]. See `doc/proposals/spec-json-schema.md`.
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
        let schema = tag_schema(&tag);
        if tag.group == "selection" {
            selection_variants.push(schema);
        } else {
            node_variants.push(schema);
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
    ])
}

fn doc_single() -> serde_json::Value {
    object(&[
        ("symbol", serde_json::json!({ "type": "string" }), true),
        ("long", def_ref("side"), false),
        ("short", def_ref("side"), false),
        ("sizing", node_ref(), false),
        ("rebalance_on", node_ref(), false),
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
    ])
}

fn doc_basket() -> serde_json::Value {
    object(&[
        ("selection", def_ref("selection"), true),
        ("score", node_ref(), true),
        ("sizing", node_ref(), true),
        ("universe", def_ref("universe"), false),
        ("rebalance_on", node_ref(), false),
        ("dollar_neutral", serde_json::json!({ "type": "boolean" }), false),
        ("long", def_ref("basket_side"), false),
        ("short", def_ref("basket_side"), false),
    ])
}

fn doc_multi() -> serde_json::Value {
    object(&[
        ("long", def_ref("multi_side"), false),
        ("short", def_ref("multi_side"), false),
        ("sizing", node_ref(), false),
        ("universe", def_ref("universe"), false),
        ("rebalance_on", node_ref(), false),
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
    ])
}

/// The schema for one tag, keyed by its `shape`.
///
/// Any tag with **no required fields** may be written bare — `close` / `!close`
/// normalise to `{close:{}}`, so both forms are accepted. That covers unit tags
/// and every all-optional map tag (the atom leaves, calendar accessors, …).
fn tag_schema(tag: &GrammarTag) -> serde_json::Value {
    use serde_json::json;
    let name = tag.name.as_str();
    let bare = json!({ "type": "string", "const": name });
    match tag.shape.as_str() {
        "map" => {
            let keyed = single_key(name, map_body(&tag.fields));
            if tag.fields.iter().any(|f| f.required) {
                keyed
            } else {
                json!({ "oneOf": [bare, keyed] })
            }
        }
        "unit" => json!({
            "oneOf": [
                bare,
                single_key(name, json!({
                    "oneOf": [{ "type": "null" }, { "type": "object", "additionalProperties": false }]
                })),
            ]
        }),
        "newtype" => single_key(name, payload_schema(tag.payload.as_deref())),
        "seq" => single_key(name, json!({ "type": "array", "items": node_ref() })),
        // Unreachable: every variant is unit/map/newtype/seq. Stay permissive.
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
fn field_schema(field: &GrammarField) -> serde_json::Value {
    let mut schema = type_fragment(&field.ty);
    if let serde_json::Value::Object(map) = &mut schema {
        if let Some(doc) = &field.doc {
            map.insert("description".to_owned(), serde_json::Value::from(doc.clone()));
        }
        if let Some(default) = &field.default {
            map.insert("default".to_owned(), default.clone());
        }
    }
    schema
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
        // `> 0` is asserted at construction; the schema stays lax (`longs`/`shorts`
        // and `every` admit 0). See the proposal's open question.
        "uint" => or_placeholder(json!({ "type": "integer", "minimum": 0 })),
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
