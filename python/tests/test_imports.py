"""Tests for `!import` confinement and the `imports=False` embedder escape
hatch on `load_spec`/`optimize` — see `fugazi::spec::imports` for the threat
model this guards against (a hosted service driving `load_spec` against
user-authored documents)."""

import pytest

import fugazi as ta


def test_load_spec_confines_absolute_import_paths(tmp_path):
    outside = tmp_path.parent / "fugazi_import_test_outside_secret.yml"
    outside.write_text("!value 1\n")
    try:
        with pytest.raises(ta.SpecError, match="outside the import root"):
            ta.load_spec(
                f"long:\n  enter: !import {outside}\n",
                base_dir=str(tmp_path),
            )
    finally:
        outside.unlink()


def test_load_spec_confines_dotdot_escapes(tmp_path):
    sub = tmp_path / "sub"
    sub.mkdir()
    outside = tmp_path.parent / "fugazi_import_test_dotdot_secret.yml"
    outside.write_text("!value 1\n")
    try:
        with pytest.raises(ta.SpecError, match="outside the import root"):
            ta.load_spec(
                f"long:\n  enter: !import ../../{outside.name}\n",
                base_dir=str(sub),
            )
    finally:
        outside.unlink()


def test_load_spec_import_within_base_dir_still_works(tmp_path):
    (tmp_path / "shared").mkdir()
    (tmp_path / "shared" / "enter.yml").write_text(
        "!gt { lhs: close, rhs: !value 10 }\n"
    )
    spec = ta.load_spec(
        "root: BTC\nlong:\n  enter: !import shared/enter.yml\n",
        base_dir=str(tmp_path),
    )
    assert spec.kind == "single"


def test_load_spec_imports_false_rejects_import_without_touching_disk():
    # No file at this path anywhere — if `imports=False` fell through to the
    # filesystem it would fail with a "no such file" error instead.
    with pytest.raises(ta.SpecError, match="disabled"):
        ta.load_spec(
            "long:\n  enter: !import definitely/does/not/exist.yml\n",
            imports=False,
        )


def test_load_spec_imports_false_rejects_import_inside_a_template_body():
    yaml = """
    universe: !all_of [BTC, ETH]
    score: !mul { lhs: !import a.yml, rhs: !value 2 }
    """
    with pytest.raises(ta.SpecError, match="disabled"):
        ta.load_spec(f"basket:\n{yaml}", kind="basket", imports=False)


def test_load_spec_imports_false_leaves_import_free_documents_unaffected():
    spec = ta.load_spec("root: BTC\nlong:\n  enter: !value true\n", imports=False)
    assert spec.kind == "single"


def test_optimize_imports_false_rejects_import():
    yaml = "root: BTC\nlong:\n  enter: !import x.yml\n"
    snaps = [ta.Snapshot({"BTC": ta.Candle(1.0, 1.0, 1.0, 1.0, 1.0)})]
    with pytest.raises(ta.SpecError, match="disabled"):
        ta.optimize(yaml, snaps, grid=[{}], imports=False)


def test_spec_grammar_host_affecting_flags_only_import():
    tags = {t["name"]: t for t in ta.spec_grammar()["tags"]}
    flagged = {name for name, t in tags.items() if t["host_affecting"]}
    assert flagged == {"import"}
