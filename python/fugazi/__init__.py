"""Python bindings for the fugazi incremental technical-analysis library.

This file exists because the package ships **type stubs** (`__init__.pyi`,
`metrics.pyi`, `montecarlo.pyi`, `py.typed`), and a wheel can only carry those
alongside a real Python package directory. maturin generates an identical shim
for a pure-extension layout; declaring `python-source` in `pyproject.toml`
hands that job here so the stubs travel with it.

Everything below re-exports the compiled `fugazi.fugazi` extension unchanged.
"""

from .fugazi import *  # noqa: F401,F403
from .fugazi import __all__, __doc__, __version__  # noqa: F401

# `import *` skips underscore-prefixed names, and the unpickling entry points
# (`_rebuild_schema`, ...) are exactly that — but a `__reduce__` resolves them as
# `fugazi._rebuild_*`, so they have to land here too. They are in the extension's
# `__all__` for this reason; see the note in `python/src/lib.rs`.
from . import fugazi as _ext

for _name in __all__:
    if _name.startswith("_"):
        globals()[_name] = getattr(_ext, _name)
del _name, _ext
