# Proposal: `spec_json_schema()` — one JSON Schema for the spec grammar

**Status:** **Both phases shipped in 0.48.0.** Phase 1 —
`spec::grammar::spec_json_schema()` / `fugazi.spec_json_schema()` (the expression
grammar). Phase 2 — `spec::grammar::spec_document_json_schema()` /
`fugazi.spec_document_json_schema()` (the whole-document envelope: the five
strategy shapes, `$ref`-ing the same node/selection grammar). Follow-up to
0.47.0's `spec_grammar()` (see [ARCHITECTURE.md](../ARCHITECTURE.md) *Parity
discipline*).

**Phase 2 notes / deviations from the sketch below.** The envelope shapes and
their slots (`SideSpec` / `BasketSideSpec` / `MultiSideSpec` / `UniverseSpec` /
`PortfolioChildSpec`) are hand-modelled in `spec::grammar` as `$defs`, each
`$ref`-ing `node`/`selection` for every expression slot — all five structs use
`deny_unknown_fields`, so the defs use `additionalProperties:false`; the root is
a `oneOf` (the shapes are disjoint by their required keys, `multi` being the
no-required fallback). Pinned by validating the real `examples/*.yml` corpus
(`strategy.yml`/`pairs.yml`/`basket.yml`) through a `!tag`→bridge transform.
Two things the sketch didn't anticipate: (1) authored per-symbol templates carry
load-time placeholders (`!arg`/`!param`/…) in **any** position, including scalar
fields (`!pick { symbol: !arg SYM }`), so the node def and every scalar field
admit a `#/$defs/placeholder` (a single-key object keyed by a
`typecheck::REWRITTEN_TAGS` name); (2) nested portfolio-child strategies are
validated only as non-empty mappings — full per-shape child validation (with
presets) is deferred.

> **Relationship to `fugazi check`.** Complementary, *not* a replacement. `check`
> (the typed parse → `typecheck.rs` → build-errors-as-values) stays the authority
> for spec correctness: it validates the type discipline and build-time semantics
> the schema can't express, over the YAML `!tag` surface. `spec_json_schema()` is
> a lightweight, language-agnostic **structural** pre-filter of the JSON bridge
> form, for consumers without the Rust build path (web form, LSP, `load_spec(dict)`).

## Why

`spec_grammar()` made serde the single authority for the spec's *presentation*
metadata (names, shapes, fields, defaults, prose). It does not, by itself,
*validate* a document. The three consumers that need machine validation —
`load_spec`, an editor LSP, and the web form — still each carry their own notion
of "is this spec well-formed." `spec_json_schema()` closes that: emit a
[JSON Schema 2020-12](https://json-schema.org/draft/2020-12) for the spec,
derived from the *same* serde definitions, so validation keys off one artifact.

The win is narrow and specific: **validation**. Codegen, docs, pills, and the
"supported-tag ⊆ grammar" conformance test are already served by
`spec_grammar()` and don't need the schema.

## The one thing to get right: which encoding are we validating?

A spec is authored as YAML with `!tag` syntax (`!sma { period: 20 }`). Standard
JSON Schema validates the **JSON data model**, and a YAML type tag has no JSON
equivalent. So the schema cannot validate the `!tag` surface directly.

What it *can* validate is the **JSON bridge encoding** that `NodeSpec`'s
`TryFrom<serde_norway::Value>` already normalises to and that the Python dict
path and the web form actually produce — an externally-tagged enum, i.e. a
**single-key object** `{ "<tag>": { <fields> } }`, plus the shorthands the
`TryFrom` accepts (`src/spec/expr.rs`, `parse_unchecked`):

| Authored form | Bridge / JSON form the schema validates |
|---|---|
| `!sma { period: 20 }` | `{ "sma": { "period": 20 } }` |
| `!close` (bare tag) / `close` (bare word) | `"close"` or `{ "close": {} }` |
| `70` / `true` | `{ "value": 70 }` (and the bare literal) |
| `[1, 2, 3]` | `{ "value": [1, 2, 3] }` |

**Scope call.** v1 targets the canonical single-key form plus the two common
shorthands (bare-string unit leaf, bare number/bool → `value`). The
load-time-only sugar (`!equal_weight`, cadence sugar `!hourly`/`!daily`, and the
`!param`/`!import`/`!arg`/`!undefined` placeholders in
`typecheck::REWRITTEN_TAGS`) is **out of scope for validation** — it is rewritten
*before* the typed parse, so a validator for the built form should reject it, and
a YAML LSP over hand-written source is a separate representation (see
*Non-goals*). Document this explicitly in the returned schema's `$comment`.

## Build it as a second projection of `spec_grammar()`

Do **not** reach for `schemars`. The recursive structure, the field types, the
required lists, the defaults, and the docs already live in `spec_grammar()` —
which is our single source and already carries the `///` prose (schemars would
reflect `NodeSpecRaw`, which has none, and add a dependency). Emit the schema by
walking the descriptor: `spec_json_schema()` becomes a *sibling* projection of
`spec_grammar()`, exactly as `spec_tags()` is.

```
spec_grammar()  ──┬──►  spec_tags()          (names)
                  └──►  spec_json_schema()    (validation)
```

### Grammar → JSON Schema mapping

Top level (`$defs/node` is the recursion anchor; node-typed fields `$ref` it):

```jsonc
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "$id": "https://fugazi/spec/0.47/node.json",
  "$defs": {
    "node":      { "oneOf": [ <one entry per group=="node" tag>, <shorthands> ] },
    "selection": { "oneOf": [ <one entry per group=="selection" tag> ] },
    "strategy":  true            // embedded-strategy leaf; opaque in v1 (see Phase 2)
  },
  "$ref": "#/$defs/node"
}
```

Per tag, keyed by `shape`:

- **map** → `{ "type":"object", "required":["<tag>"], "additionalProperties":false,
  "properties": { "<tag>": { "type":"object", "additionalProperties":false,
  "properties": {<fields>}, "required":[<required fields>] } } }`
- **unit** → the single-key `{ "<tag>": {} }` form **and** the bare-string form
  `{ "const":"<tag>" }` (union the two).
- **newtype** → `{ "<tag>": <payload schema> }` (e.g. `not` → node-ref; `value`
  → `{ "oneOf":[{"type":"number"},{"type":"string"},{"type":"boolean"}] }`).
- **seq** → `{ "<tag>": { "type":"array", "items": <item schema> } }`.

Per field, keyed by `type`:

| grammar `type` | JSON Schema fragment |
|---|---|
| `node` | `{ "$ref": "#/$defs/node" }` |
| `node_list` | `{ "type":"array", "items": { "$ref":"#/$defs/node" } }` |
| `uint` | `{ "type":"integer", "minimum": 1 }` *(periods are `> 0`; see open Q)* |
| `number` | `{ "type":"number" }` |
| `str` | `{ "type":"string" }` |
| `bool` | `{ "type":"boolean" }` |
| `str_operand` | `{ "oneOf":[{"type":"string"},{"$ref":"#/$defs/node"}] }` *(matches `StrOperand::{Literal, Expr}`)* |
| `match_cases` | `{ "type":"array", "items": { "$ref":"#/$defs/match_case" } }` |
| `strategy` | `{ "$ref":"#/$defs/strategy" }` |
| `selection` | `{ "$ref":"#/$defs/selection" }` |
| `other` | `true` *(and `log()` a warning; `other` should be driven to zero — it means a field type the mapper doesn't model)* |

Carry `field.doc`/`tag.doc` → `"description"`, `field.default` → `"default"`,
`field.required` → the parent `required` array. `additionalProperties:false`
mirrors serde's `deny_unknown_fields` (the whole reason `NodeSpecRaw` denies
them), so a typo'd key fails the same way it does at parse.

## Phased delivery

**Phase 1 — the expression grammar (this proposal's core).** The recursive
`node` schema + the `selection` schema, from `spec_grammar()`. Validates any
node/overlay expression in the JSON bridge form. Directly serves the web form's
pill/overlay builders and dict-form `load_spec`/`compute_overlays` validation. No
new reflection — pure projection. Small, shippable.

**Phase 2 — the document envelope (separate change).** The five document shapes
(single / pairs / basket / multi / portfolio) and their slots (`symbol`,
`long`/`short` → `enter`/`exit`/`stop_loss`/`take_profit`, `sizing`, `selection`,
`universe`, `children`, `weights`, `rebalance_on`, `costs`) are **not** in
`spec_grammar()` today — they're the `*StrategySpec` structs. A whole-document
schema needs those reflected too. Two options, decide when we start Phase 2:
extend the `SpecGrammar` reflection to the strategy structs (keeps one artifact),
or `schemars`-derive just the envelope structs and `$ref` the Phase-1 `node`
schema for every expression slot. Ship Phase 1 first; it's the load-bearing part.

## API surface

- Core: `pub fn spec::grammar::spec_json_schema() -> serde_json::Value`.
- Python: `#[pyfunction] fugazi.spec_json_schema() -> dict` (reuse `json_to_py`,
  register in `lib.rs` next to `spec_grammar`).
- Versioning: put the crate version in `$id` and keep the existing
  `SCHEMA_VERSION` guarding descriptor *shape*; the schema is regenerated, so it
  tracks the grammar automatically.
- Optionally wire into `load_spec`/`check` as a pre-parse lint that produces a
  JSON-Pointer error path (nice-to-have; the typed parse already reports a
  `!tag > ` breadcrumb, so this is additive, not a replacement).

## Testing

- **Rust** (`tests/spec_json_schema.rs`): the schema is valid JSON; every `$ref`
  resolves; there is exactly one `$defs/node` entry per `group=="node"` tag and
  one `$defs/selection` per selection tag (drives `other` → 0); a `oneOf` covers
  every tag name in `spec_grammar()`.
- **Constructive conformance** (strongest, no external corpus needed): for every
  tag, synthesise a minimal valid instance *from the descriptor itself* (required
  scalar fields → defaults or dummy values, node fields → `"close"`) and assert
  it validates; mutate one required key away / add an unknown key and assert it
  fails. This pins the schema to the grammar with zero hand-written fixtures.
- **Python** (`jsonschema` lib): validate the dict-form specs already exercised
  in `python/tests/test_specs.py`, and validate the `examples/*.yml` corpus
  (`basket.yml`, `binance.yml`, `ibkr.yml`, `pairs.yml`) after converting their
  `!tag` nodes to the single-key bridge form with a small YAML-tag → `{tag: body}`
  transform (≈20 lines; the inverse of what the pills render).
- **Downstream contract**: the web service's conformance test can then assert its
  overlay-builder subset validates against `spec_json_schema()`, so bumping the
  wheel fails CI loudly on a newly-added indicator instead of 422-ing at runtime.

## Non-goals / open questions

- **YAML `!tag` LSP.** A completion/validation experience over *hand-written*
  YAML needs a YAML-tag-aware schema (e.g. a `yaml-language-server` custom tag
  set), which standard JSON Schema doesn't express. The descriptor still powers
  it (tag names, per-tag field names + required + docs for completion); it just
  isn't *this* artifact. Call this out so nobody expects `.yml` red squiggles for
  free.
- **`uint` minimum.** Constructors `assert!(period > 0)`, so `minimum: 1` is
  right for period-like fields — but `uint` also covers `longs`/`shorts`
  (`0` is valid) and `every`. Options: `minimum: 0` for all (lax, matches serde),
  or a per-field `min` hint added to `GrammarField` (tighter, but grows the
  descriptor). Recommend `minimum: 0` in v1; revisit if a tighter bound earns its
  keep.
- **`match_cases`.** Needs a `$defs/match_case` (`{ when, then }`); confirm the
  `MatchCase` shape and whether patterns are homogeneously typed at the schema
  level (they're checked at build, not necessarily expressible in JSON Schema).
- **Polymorphic outputs.** `output: "any"` tags (`value`, `get`, `if_else`,
  `match`) are structurally valid anywhere a node is; JSON Schema can't enforce
  the Real-vs-Bool-vs-Str *type* discipline that `typecheck.rs` does at build.
  The schema validates *structure*; type-correctness stays with `typecheck`.
  Document the division so the schema isn't mistaken for a full type checker.

## Effort

Phase 1 is ~a day: the projection (~150 lines), the Python binding (~10),
the Rust constructive-conformance test (~120), the Python `jsonschema` test
(~40), docs. Phase 2 is materially larger and gated on the envelope-reflection
decision above.
