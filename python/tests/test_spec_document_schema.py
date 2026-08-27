"""`spec_document_json_schema()` must validate real, whole strategy documents.

The strongest test uses the actual `examples/*.yml` corpus: load each with a
`!tag` → `{tag: body}` bridge transform (the same normalisation `NodeSpec`'s
`TryFrom` does) and assert it validates against the document schema as exactly
one of the five shapes — plus minimal synthesised `multi`/`portfolio` docs,
which have no example file, and negative cases.
"""

from pathlib import Path

import pytest

import fugazi as ta

jsonschema = pytest.importorskip("jsonschema")
yaml = pytest.importorskip("yaml")

_EXAMPLES = Path(__file__).resolve().parents[2] / "examples"


class _BridgeLoader(yaml.SafeLoader):
    """A SafeLoader that turns every `!tag value` into `{tag: value}` — the JSON
    bridge form the schema validates (and `NodeSpec`'s TryFrom normalises to)."""


def _bridge(loader, tag_suffix, node):
    if isinstance(node, yaml.MappingNode):
        return {tag_suffix: loader.construct_mapping(node, deep=True)}
    if isinstance(node, yaml.SequenceNode):
        return {tag_suffix: loader.construct_sequence(node, deep=True)}
    raw = node.value
    # Re-resolve the scalar's type (int/float/bool/str); a bare tag (`!never`)
    # has an empty scalar and becomes `{tag: {}}`.
    value = yaml.safe_load(raw) if raw != "" else {}
    return {tag_suffix: value}


_BridgeLoader.add_multi_constructor("!", _bridge)


def _load(path):
    return yaml.load(path.read_text(), Loader=_BridgeLoader)


# example file -> the shape it should validate as
_CORPUS = {
    "strategy.yml": "single",
    "pairs.yml": "pairs",
    "basket.yml": "basket",
}

# shapes with no example file — a minimal hand-written instance each.
_MINIMAL = {
    "multi": {"long": {"enter": {"gt": {"lhs": "close", "rhs": {"value": 0}}}}},
    "portfolio": {"children": [{"name": "a", "strategy": {"symbol": "BTC"}}]},
}


def _which_shape(schema, doc):
    """Every shape def `doc` validates against.

    Usually exactly one. Not always: `root:` is optional on the single-asset
    shape (it defaults to `!pick { symbol: !param SYMBOL, freq: !param FREQ }`),
    and a document that omits it is a bare `long:` / `short:` map — which is
    also exactly what a `multi:` document is. The schema's root is an `anyOf`
    for that reason, and the shape comes from the caller.
    """
    matched = []
    for branch in schema["anyOf"]:
        name = branch["$ref"].split("/")[-1]
        sub = {k: v for k, v in schema.items() if k != "anyOf"}
        sub["$ref"] = branch["$ref"]
        if jsonschema.Draft202012Validator(sub).is_valid(doc):
            matched.append(name)
    return matched


def test_document_schema_is_valid():
    schema = ta.spec_document_json_schema()
    jsonschema.Draft202012Validator.check_schema(schema)


def test_example_documents_validate_as_their_shape():
    schema = ta.spec_document_json_schema()
    validator = jsonschema.Draft202012Validator(schema)
    for fname, shape in _CORPUS.items():
        doc = _load(_EXAMPLES / fname)
        errors = list(validator.iter_errors(doc))
        assert not errors, (
            f"{fname} failed the document schema: {[e.message for e in errors]}"
        )
        assert _which_shape(schema, doc) == [shape], (
            f"{fname} should validate as exactly [{shape}], got {_which_shape(schema, doc)}"
        )


def test_minimal_multi_and_portfolio_validate():
    schema = ta.spec_document_json_schema()
    for shape, doc in _MINIMAL.items():
        assert shape in _which_shape(schema, doc), (
            f"minimal {shape} doc matched {_which_shape(schema, doc)}"
        )
        assert jsonschema.Draft202012Validator(schema).is_valid(doc)


def test_a_rootless_document_is_both_a_single_and_a_multi():
    """The one place two shapes overlap, and why the root is an `anyOf`.

    `root:` is optional on the single-asset shape, so a document that omits it
    is indistinguishable from a `multi:` one — both are a bare `long:` /
    `short:` map. The schema must accept it (it is a valid document either way)
    rather than reject it for matching twice, which is what `oneOf` would do.
    """
    schema = ta.spec_document_json_schema()
    doc = _MINIMAL["multi"]
    assert set(_which_shape(schema, doc)) == {"single", "multi"}
    assert jsonschema.Draft202012Validator(schema).is_valid(doc)
    # Spelled out, it is unambiguously a single-asset document again.
    rooted = dict(doc, root="BTC")
    assert _which_shape(schema, rooted) == ["single"]


def test_the_single_shape_publishes_its_default_root():
    """`root:` is optional, and the schema says what omitting it means.

    A consumer that renders "defaults to …" reads this rather than hardcoding
    the expansion. It is the pre-substitution value — the two `!param`
    placeholders are what the loader splices, and an unset one drops its key.

    Both declare a `type:`, and that is part of the published contract rather
    than an implementation detail: it is what stringifies `SYMBOL=123` into a
    numeric ticker and what refuses `FREQ=1hh` at load, on the one path that has
    no placeholder body of its own to hang a declaration on.
    """
    schema = ta.spec_document_json_schema()
    single = schema["$defs"]["single"]
    assert "root" not in single["required"]
    assert single["properties"]["root"]["default"] == {
        "pick": {
            "symbol": {"param": {"key": "SYMBOL", "default": None, "type": "symbol"}},
            "freq": {"param": {"key": "FREQ", "default": None, "type": "frequency"}},
        }
    }


def test_malformed_documents_are_rejected():
    node = jsonschema.Draft202012Validator(ta.spec_document_json_schema())
    # Unknown top-level key (deny_unknown_fields → additionalProperties:false).
    assert not node.is_valid({"symbol": "BTC", "bogus": 1})
    # A doc that mixes two shapes' discriminators matches neither.
    assert not node.is_valid({"symbol": "BTC", "left": "A", "right": "B"})
    # A required slot is missing its required sub-field (side.enter).
    assert not node.is_valid({"symbol": "BTC", "long": {"exit": "close"}})
