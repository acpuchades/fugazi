"""Type stubs for `fugazi.montecarlo`. GENERATED — see tools/gen_python_stubs.py."""

from typing import Any

def resample_index_matrix(n: Any, permutations: int, *, scheme: str = ..., block: float = ..., seed: int = ...) -> Any:
    """`permutations` same-length resampling index sequences into `0..n`, drawn in
    order from a single stream seeded by `seed`.
    """
    ...
def resample_indices(n: Any, *, scheme: str = ..., block: float = ..., seed: int = ...) -> Any:
    """One same-length resampling index sequence into `0..n` under `scheme`."""
    ...
