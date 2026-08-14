"""`spec_json_schema()` must be a valid JSON Schema that accepts a well-formed
instance of every tag and rejects malformed ones — validated against a real
engine (`jsonschema`), with the instances *constructed from the descriptor* so
there are no hand-written fixtures to drift.
"""

import copy

import pytest

import fugazi as ta

jsonschema = pytest.importorskip("jsonschema")


# A minimal valid value for each grammar field/payload type.
def _dummy(ty):
    return {
        "node": "close",
        "node_list": ["close"],
        "uint": 1,
        "number": 1.0,
        "str": "x",
        "bool": True,
        "str_operand": "x",
        "match_cases": [{"when": 1, "value": "close"}],
        "strategy": {},
        "selection": "everything",
        "literal": 1,
        "other": 1,
    }[ty]


def _valid_instance(tag):
    """A minimal instance of `tag` in the JSON bridge form."""
    name = tag["name"]
    shape = tag["shape"]
    if shape == "unit":
        return name  # bare-string form
    if shape == "map":
        body = {f["name"]: _dummy(f["type"]) for f in tag["fields"] if f["required"]}
        return {name: body}
    if shape == "newtype":
        return {name: _dummy(tag["payload"])}
    if shape == "seq":
        return {name: [_dummy("node")]}
    raise AssertionError(f"unknown shape {shape!r}")


def _root_for(group, schema):
    """The schema to validate an instance of `group` against (its root $ref)."""
    root = dict(schema)
    root["$ref"] = "#/$defs/node" if group == "node" else "#/$defs/selection"
    return root


def test_schema_is_a_valid_draft_2020_12_schema():
    schema = ta.spec_json_schema()
    # Raises SchemaError if the emitted document isn't a legal schema.
    jsonschema.Draft202012Validator.check_schema(schema)
    assert schema["$ref"] == "#/$defs/node"
    assert set(schema["$defs"]) >= {"node", "selection", "match_case", "strategy"}


def test_every_tag_has_a_valid_minimal_instance():
    schema = ta.spec_json_schema()
    tags = ta.spec_grammar()["tags"]
    validators = {
        g: jsonschema.Draft202012Validator(_root_for(g, schema))
        for g in ("node", "selection")
    }
    failures = []
    for tag in tags:
        instance = _valid_instance(tag)
        errors = list(validators[tag["group"]].iter_errors(instance))
        if errors:
            failures.append(f"!{tag['name']} ({tag['shape']}): {errors[0].message}")
    assert not failures, "these tags' minimal instances failed validation:\n  " + "\n  ".join(
        failures
    )


def test_unknown_tag_and_unknown_field_are_rejected():
    schema = ta.spec_json_schema()
    node = jsonschema.Draft202012Validator(schema)

    # An unknown tag matches no branch of the node oneOf.
    assert not node.is_valid({"__not_a_real_tag__": {}})
    assert not node.is_valid("__not_a_real_tag__")

    # additionalProperties:false mirrors serde's deny_unknown_fields: an unknown
    # key inside a real tag's body is rejected. Use a map tag to prove it.
    map_tag = next(
        t
        for t in ta.spec_grammar()["tags"]
        if t["group"] == "node" and t["shape"] == "map"
    )
    good = _valid_instance(map_tag)
    bad = copy.deepcopy(good)
    bad[map_tag["name"]]["__nope__"] = 1
    assert node.is_valid(good), f"{map_tag['name']} should be valid before mutation"
    assert not node.is_valid(bad), "an unknown field should be rejected"


def test_bare_literal_shorthands_validate_as_nodes():
    node = jsonschema.Draft202012Validator(ta.spec_json_schema())
    for literal in (70, 3.14, True, [1.0, 2.0, 3.0]):
        assert node.is_valid(literal), f"bare {literal!r} should validate as a node"
    # A wrapped constant is equally valid.
    assert node.is_valid({"value": 70})
