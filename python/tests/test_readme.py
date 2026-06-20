"""Execute every ```python code block in the READMEs so the docs can't drift.

Each block runs in a fresh namespace pre-seeded with the illustrative names the
snippets use (``df``, ``bars``, ``stream``, ``fast``/``slow``, ...), mirroring
what a reader would have in scope.
"""

import pathlib
import re

import pytest

ROOT = pathlib.Path(__file__).resolve().parent.parent
READMES = [ROOT / "README.md", ROOT.parent / "README.md"]

BLOCK = re.compile(r"```python\n(.*?)```", re.DOTALL)


def make_env():
    """A fresh namespace seeded like a reader's session."""
    pd = pytest.importorskip("pandas")
    np = pytest.importorskip("numpy")
    import arcana as ta

    prices = [100.0 + i * 0.5 for i in range(40)]
    bars = [(p, p + 1.0, p - 1.0, p, 100.0) for p in prices]
    stream = [ta.Candle(*b) for b in bars]
    df = pd.DataFrame(bars, columns=["open", "high", "low", "close", "volume"])

    env = {
        "ta": ta,
        "pd": pd,
        "np": np,
        "df": df,
        "bars": bars,
        "stream": stream,
        "candles": stream,
        "prices": prices,
        "prices_list": prices,
        "series1": prices[:20],
        "series2": prices[20:],
        # illustrative operands used by API-fragment snippets
        "node": ta.ema(ta.close(), 5),
        "other": ta.sma(ta.close(), 5),
        "fast": ta.ema(ta.close(), 5),
        "slow": ta.ema(ta.close(), 10),
        "src": ta.close(),
        "a": ta.close().above(1.0),
        "b": ta.close().below(1e9),
        "c": ta.close().above(2.0),
        "candle": ta.Candle(1.0, 2.0, 0.5, 1.5, 100.0),
    }
    try:
        import polars as pl

        env["pl"] = pl
    except ImportError:
        pass
    return env


def _blocks():
    cases = []
    for readme in READMES:
        for i, block in enumerate(BLOCK.findall(readme.read_text())):
            cases.append(pytest.param(block, id=f"{readme.parent.name}/README.md#py{i}"))
    return cases


@pytest.mark.parametrize("block", _blocks())
def test_readme_python_block_runs(block):
    pytest.importorskip("pandas")
    pytest.importorskip("numpy")
    # Strip Markdown blockquote markers so quoted code blocks are valid Python,
    # without touching indentation of ordinary blocks.
    def unquote(line):
        if line.startswith("> "):
            return line[2:]
        return "" if line == ">" else line

    block = "\n".join(unquote(line) for line in block.splitlines())
    env = make_env()
    try:
        exec(compile(block, "<readme>", "exec"), env)
    except Exception as exc:  # pragma: no cover - failure detail
        pytest.fail(f"README example failed: {type(exc).__name__}: {exc}\n---\n{block}")
