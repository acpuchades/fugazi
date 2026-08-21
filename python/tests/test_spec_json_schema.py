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
        # A period or window length — `NonZeroUsize` on the spec side, so the
        # schema says `minimum: 1` and 0 would not be a valid instance.
        "positive_uint": 1,
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


def _valid_instance(tag, form=None):
    """A minimal instance of `tag` in the JSON bridge form.

    `form` selects which spelling to build; the canonical one by default. A tag
    accepts every entry of `forms`, so the schema has to validate every entry —
    see `test_every_declared_form_validates`.
    """
    name = tag["name"]
    form = form if form is not None else tag["forms"][0]
    shape = form["shape"]
    if shape == "unit":
        return name  # bare-string form
    if shape == "map":
        body = {f["name"]: _dummy(f["type"]) for f in form["fields"] if f["required"]}
        return {name: body}
    if shape == "newtype":
        return {name: _dummy(form["payload"])}
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
        # Only the expression vocabularies are in this schema; the document-level
        # groups (universe / weighting / document) are directives, not nodes.
        if tag["group"] not in validators:
            continue
        instance = _valid_instance(tag)
        errors = list(validators[tag["group"]].iter_errors(instance))
        if errors:
            failures.append(f"!{tag['name']} ({tag['shape']}): {errors[0].message}")
    assert not failures, "these tags' minimal instances failed validation:\n  " + "\n  ".join(
        failures
    )


def test_node_slot_defaults_validate():
    """Every default the schema advertises is a valid instance of its own slot.

    The descriptor reports a node default as a YAML fragment (`{"expr":
    "!close"}`); the schema has to report the same fact in *its* encoding, the
    JSON bridge form (`"close"`) — the one it validates. Two encodings of one
    fact is two chances to be wrong, so this validates each advertised `default`
    against the slot's own schema. A literal is checked the same way, which also
    pins the older half of the claim (`!macd_line`'s `fast: 12` really is a
    legal `positive_uint`).
    """
    node = jsonschema.Draft202012Validator(ta.spec_json_schema())
    checked = 0
    for tag in ta.spec_grammar()["tags"]:
        if tag["group"] != "node":
            continue
        for form in tag["forms"]:
            for field in form["fields"]:
                if field["default"] is None:
                    continue
                declared = _declared_default(node.schema, tag, form, field)
                assert declared is not _MISSING, (
                    f"!{tag['name']}.{field['name']} has a descriptor default "
                    f"{field['default']!r} but the schema advertises none"
                )
                # Fill the optional key with the schema's own advertised default
                # and require the whole tag to still validate.
                instance = _valid_instance(tag, form)
                instance[tag["name"]][field["name"]] = declared
                assert node.is_valid(instance), (
                    f"!{tag['name']}.{field['name']} defaults to {declared!r}, which "
                    f"the schema rejects in that slot"
                )
                checked += 1
    assert checked >= 90, f"only {checked} advertised defaults were reachable"


_MISSING = object()


def _declared_default(schema, tag, form, field):
    """The `default` the JSON Schema attaches to one tag's field, or `_MISSING`."""
    for branch in _tag_branches(schema["$defs"]["node"]["oneOf"], tag["name"]):
        for props in _bodies(branch["properties"][tag["name"]]):
            if "default" in props.get(field["name"], {}):
                return props[field["name"]]["default"]
    return _MISSING


def _bodies(body):
    """The `properties` map(s) of a tag body, through the null-or-object union.

    A `map` tag with no required key accepts an omitted body — the bare string,
    or an explicit null — so its body schema is a union and the keys sit one
    level in.
    """
    if "properties" in body:
        yield body["properties"]
    for arm in body.get("oneOf", []):
        yield from _bodies(arm)


def _tag_branches(branches, name):
    """Every `{name: body}` object branch for `name`, through nested unions."""
    for branch in branches:
        for key in ("oneOf", "anyOf"):
            if key in branch:
                yield from _tag_branches(branch[key], name)
        if name in branch.get("properties", {}):
            yield branch


def test_all_optional_map_tags_accept_an_explicit_null_body():
    """`{"close": null}` is what a YAML `!close` normalises to, and the parser
    takes it — so the schema must too.

    The `map` arm used to admit only the bare string and a real object body, so
    a tag with no required key rejected the very form the loader produces. The
    `unit` arm always had the null branch; this is the same claim one shape
    over. The parser half is pinned in `tests/spec_grammar.rs`.
    """
    node = jsonschema.Draft202012Validator(ta.spec_json_schema())
    checked = 0
    for tag in ta.spec_grammar()["tags"]:
        if tag["group"] != "node":
            continue
        for form in tag["forms"]:
            if form["shape"] != "map" or any(f["required"] for f in form["fields"]):
                continue
            assert node.is_valid({tag["name"]: None}), (
                f"the schema rejects {{{tag['name']!r}: null}}, which fugazi parses"
            )
            checked += 1
    assert checked >= 30, f"only {checked} all-optional map forms were reachable"


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
        if t["group"] == "node" and t["forms"][0]["shape"] == "map"
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


def test_every_declared_form_validates():
    """The schema accepts every spelling the descriptor declares.

    A tag with more than one form (`!changed <node>` and `!changed { source }`,
    `!unstable { source }` and bare `!unstable <node>`) is emitted as an `anyOf`
    over its forms. Before v5 the schema knew one shape per tag and rejected the
    other — so a consumer generating a document from the descriptor's alternate
    spelling produced something the schema called invalid and fugazi accepted.
    """
    node = jsonschema.Draft202012Validator(ta.spec_json_schema())
    multi = 0
    for tag in ta.spec_grammar()["tags"]:
        if tag["group"] != "node":
            continue
        if len(tag["forms"]) > 1:
            multi += 1
        for i, form in enumerate(tag["forms"]):
            try:
                instance = _valid_instance(tag, form)
            except KeyError:
                continue  # a field type with no dummy (an embedded strategy)
            assert node.is_valid(instance), (
                f"!{tag['name']} form[{i}] ({form['shape']}) is declared but the "
                f"schema rejects it: {instance!r}"
            )
    assert multi >= 4, "the four unary wrappers each declare two spellings"
