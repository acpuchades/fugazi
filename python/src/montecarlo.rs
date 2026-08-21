use crate::prelude::*;
// The binding modules were one flat namespace before the split and still read
// as one: each pulls in its siblings, so a cross-module reference needs no path.
#[allow(unused_imports)]
use crate::carriers::*;
#[allow(unused_imports)]
use crate::classes::*;
#[allow(unused_imports)]
use crate::constructors::*;
#[allow(unused_imports)]
use crate::metrics::*;
#[allow(unused_imports)]
use crate::sources::*;
#[allow(unused_imports)]
use crate::spec::*;
#[allow(unused_imports)]
use crate::strategy::*;

use fugazi_core::montecarlo::{resample_indices as core_resample_indices, rng_from_seed};

// ---------------------------------------------------------------------------
// Monte Carlo: expose the deterministic resampling primitive as the
// `fugazi.montecarlo` submodule.
//
// The significance layer (`StrategySpec.evaluate(montecarlo=...)`) reduces every
// resample to metric rows and throws the resampled *paths* away. That is the
// right default for a pickled process-pool result — but it means a consumer who
// wants a Monte Carlo equity fan chart (percentile bands of the resampled equity
// paths over time) has nothing to build it from.
//
// Rather than grow a bespoke "bands" product on the evaluate call, this module
// exposes the one generic knob it takes to rebuild *any* path statistic
// yourself: the resampling index draws. They are pure, seed-deterministic, and
// tiny to produce, so the reconstruction can happen wherever you like (even
// outside the worker) from scalar inputs — nothing large crosses the boundary.
//
// The bootstrap-CI estimator draws first from the run's seed stream, in the same
// order and via the same primitive, so `resample_index_matrix(len(returns), P,
// scheme=..., block=..., seed=...)` reproduces exactly the permutations whose
// metrics land in the `montecarlo` CIs. The equity fan is then five lines the
// consumer owns:
//
// ```python
// import numpy as np, fugazi
// rep = spec.run(wallet, snaps)
// r   = np.array(fugazi.metrics.per_bar_returns(rep.equity_curve, rep.initial_equity))
// idx = np.array(fugazi.montecarlo.resample_index_matrix(
//         len(r), 1000, scheme="stationary", block=10, seed=0))
// paths = rep.initial_equity * np.cumprod(1 + r[idx], axis=1)     # (P × bars)
// bands = {f"p{q}": np.percentile(paths, q, axis=0).tolist() for q in (5, 25, 50, 75, 95)}
// ```
//
// Each resampled series is the same length as the source (every scheme produces
// a same-length synthetic series), so `paths[:, k]` maps 1:1 onto the original
// bar `k` — bands drop straight onto the real timestamps.
// ---------------------------------------------------------------------------

/// Parse the `scheme` / `block` pair into a core [`ResampleScheme`], mirroring
/// `MonteCarloConfig`'s parsing so the two surfaces stay in step.
fn parse_scheme(scheme: &str, block: f64) -> PyResult<ResampleScheme> {
    match scheme {
        "iid" => Ok(ResampleScheme::Iid),
        "moving-block" | "moving_block" => Ok(ResampleScheme::MovingBlock {
            block: block.max(1.0) as usize,
        }),
        "stationary" => Ok(ResampleScheme::Stationary { mean_block: block }),
        other => Err(PyValueError::new_err(format!(
            "unknown scheme `{other}` (expected iid | moving-block | stationary)"
        ))),
    }
}

/// One same-length resampling index sequence into `0..n` under `scheme`.
///
/// Every index is in `0..n`, so it can gather any length-`n` series (e.g. a
/// run's per-bar returns) into one synthetic path. Deterministic in `seed`:
/// this is permutation `0` of [`resample_index_matrix`] with the same
/// arguments. `scheme` is one of `iid` / `moving-block` / `stationary`
/// (default); `block` is the block length (moving-block) or expected block
/// length (stationary), ignored for `iid`.
#[pyfunction]
#[pyo3(signature = (n, *, scheme = "stationary", block = 10.0, seed = 0))]
pub(crate) fn resample_indices(
    n: usize,
    scheme: &str,
    block: f64,
    seed: u64,
) -> PyResult<Vec<usize>> {
    let scheme = parse_scheme(scheme, block)?;
    let mut rng = rng_from_seed(seed);
    Ok(core_resample_indices(n, scheme, &mut rng))
}

/// `permutations` same-length resampling index sequences into `0..n`, drawn in
/// order from a single stream seeded by `seed`.
///
/// Returns a `permutations × n` matrix. Because the run's bootstrap-CI estimator
/// draws first from the same seed stream via the same primitive, calling this
/// with `n = len(returns)` and the run's `permutations` / `scheme` / `block` /
/// `seed` reproduces exactly the permutations behind
/// `evaluate(montecarlo=...)`'s confidence intervals — so the equity paths you
/// rebuild (`initial * cumprod(1 + returns[idx], axis=1)`) are the same ones
/// those CIs summarize. Deterministic and portable (ChaCha8).
#[pyfunction]
#[pyo3(signature = (n, permutations, *, scheme = "stationary", block = 10.0, seed = 0))]
pub(crate) fn resample_index_matrix(
    n: usize,
    permutations: usize,
    scheme: &str,
    block: f64,
    seed: u64,
) -> PyResult<Vec<Vec<usize>>> {
    let scheme = parse_scheme(scheme, block)?;
    let mut rng = rng_from_seed(seed);
    Ok((0..permutations)
        .map(|_| core_resample_indices(n, scheme, &mut rng))
        .collect())
}

/// Register the resampling primitives on the `fugazi.montecarlo` submodule.
pub(crate) fn register_montecarlo_module(m: &Bound<'_, PyModule>) -> PyResult<()> {
    macro_rules! reg {
        ($($f:ident),* $(,)?) => { $( m.add_function(wrap_pyfunction!($f, m)?)?; )* };
    }
    reg!(resample_indices, resample_index_matrix);
    Ok(())
}
