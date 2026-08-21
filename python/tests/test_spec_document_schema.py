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
    """The shape def(s) `doc` validates against — should be exactly one."""
    matched = []
    for branch in schema["oneOf"]:
        name = branch["$ref"].split("/")[-1]
        sub = dict(schema)
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
        assert _which_shape(schema, doc) == [shape], (
            f"minimal {shape} doc matched {_which_shape(schema, doc)}"
        )


def test_malformed_documents_are_rejected():
    node = jsonschema.Draft202012Validator(ta.spec_document_json_schema())
    # Unknown top-level key (deny_unknown_fields → additionalProperties:false).
    assert not node.is_valid({"symbol": "BTC", "bogus": 1})
    # A doc that mixes two shapes' discriminators matches neither.
    assert not node.is_valid({"symbol": "BTC", "left": "A", "right": "B"})
    # A required slot is missing its required sub-field (side.enter).
    assert not node.is_valid({"symbol": "BTC", "long": {"exit": "close"}})
