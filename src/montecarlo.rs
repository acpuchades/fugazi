//! Resampling primitives for Monte Carlo significance analysis.
//!
//! This module is deliberately small and pure: it turns a seed into a
//! reproducible stream of *index sequences* under one of three resampling
//! schemes, and applies them to a slice. The statistical layer that turns
//! those resamples into confidence intervals and empirical-null p-values lives
//! in [`crate::spec::montecarlo`], which drives whole backtests and therefore
//! needs the `spec` feature; everything here needs only `rand`.
//!
//! ## Why the scheme matters
//!
//! An **IID** bootstrap ([`ResampleScheme::Iid`]) draws each output element
//! independently, which destroys *all* serial dependence — autocorrelation and
//! volatility clustering alike. For financial returns that biases a bootstrap
//! toward a too-tight sampling distribution (over-narrow CIs) and a too-tight
//! null (overstated significance). **Block** resampling keeps runs of
//! consecutive observations together, preserving short-range dependence:
//!
//! * [`ResampleScheme::MovingBlock`] — fixed-length blocks, overlapping and
//!   circular. Simple; the block length is literal.
//! * [`ResampleScheme::Stationary`] — Politis–Romano (1994): geometric random
//!   block lengths with a user-set *expected* length. The resampled series is
//!   genuinely stationary (no dependence on where the blocks were cut), which
//!   is why it is the default for time-series resampling here.
//!
//! All three produce a same-length synthetic series, so downstream metrics see
//! the same number of observations as the original.

#[cfg(feature = "montecarlo")]
use rand::{RngExt, SeedableRng};
#[cfg(feature = "montecarlo")]
use rand_chacha::ChaCha8Rng;

use crate::market::Real;

/// The seeded RNG backing every Monte Carlo resample. ChaCha8 is a portable,
/// deterministic stream given a `u64` seed, so a reported CI or p-value
/// reproduces bit-for-bit on any platform.
#[cfg(feature = "montecarlo")]
pub type McRng = ChaCha8Rng;

/// Build the resampling RNG for a run from its seed.
#[cfg(feature = "montecarlo")]
pub fn rng_from_seed(seed: u64) -> McRng {
    ChaCha8Rng::seed_from_u64(seed)
}

/// How a series is resampled into a same-length synthetic series.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ResampleScheme {
    /// IID bootstrap: each output element is an independent uniform draw.
    /// Equivalent to a moving block of length 1; destroys serial dependence.
    Iid,
    /// Moving-block bootstrap: concatenate fixed-length runs of `block`
    /// consecutive elements (chosen circularly), preserving dependence within a
    /// block. `block` is clamped to `[1, n]` at resample time.
    MovingBlock { block: usize },
    /// Stationary bootstrap (Politis–Romano 1994): after each element, start a
    /// new random block with probability `1 / mean_block`, else advance one
    /// step (circularly). Block lengths are geometric with mean `mean_block`
    /// and the resampled series is stationary. `mean_block` is clamped to
    /// `>= 1.0`.
    Stationary { mean_block: Real },
}

impl ResampleScheme {
    /// A short human-readable tag echoed into `metrics.yml`'s montecarlo block,
    /// e.g. `"stationary(mean_block=10)"`.
    pub fn label(&self) -> String {
        match *self {
            ResampleScheme::Iid => "iid".to_string(),
            ResampleScheme::MovingBlock { block } => format!("moving_block(block={block})"),
            ResampleScheme::Stationary { mean_block } => {
                format!("stationary(mean_block={mean_block})")
            }
        }
    }
}

/// Draw one same-length index sequence into `0..n` under `scheme`.
///
/// Returns an empty vector when `n == 0`. Every index is in `0..n`, so the
/// result can index any slice of length `n` without bounds checks.
#[cfg(feature = "montecarlo")]
pub fn resample_indices(n: usize, scheme: ResampleScheme, rng: &mut McRng) -> Vec<usize> {
    if n == 0 {
        return Vec::new();
    }
    match scheme {
        ResampleScheme::Iid => (0..n).map(|_| rng.random_range(0..n)).collect(),
        ResampleScheme::MovingBlock { block } => {
            let block = block.clamp(1, n);
            let mut out = Vec::with_capacity(n);
            while out.len() < n {
                let start = rng.random_range(0..n);
                for k in 0..block {
                    out.push((start + k) % n);
                    if out.len() == n {
                        break;
                    }
                }
            }
            out
        }
        ResampleScheme::Stationary { mean_block } => {
            // p = 1/mean_block is the per-step probability of starting a fresh
            // block; mean_block < 1 is meaningless (would exceed prob 1), so
            // clamp it — mean_block == 1 reduces to the IID bootstrap.
            let p = 1.0 / mean_block.max(1.0);
            let mut out = Vec::with_capacity(n);
            let mut idx = rng.random_range(0..n);
            for _ in 0..n {
                out.push(idx);
                if rng.random::<f64>() < p {
                    idx = rng.random_range(0..n);
                } else {
                    idx = (idx + 1) % n;
                }
            }
            out
        }
    }
}

/// Resample `data` into a same-length synthetic series under `scheme`.
#[cfg(feature = "montecarlo")]
pub fn resample_slice<T: Clone>(data: &[T], scheme: ResampleScheme, rng: &mut McRng) -> Vec<T> {
    resample_indices(data.len(), scheme, rng)
        .into_iter()
        .map(|i| data[i].clone())
        .collect()
}

/// The one-sided percentile of `values` at fraction `p` (R type-7, linear
/// interpolation) — the quantile convention the crate uses everywhere. `values`
/// need not be sorted; this sorts a copy. Returns `None` on an empty slice.
pub fn percentile(values: &[Real], p: Real) -> Option<Real> {
    if values.is_empty() {
        return None;
    }
    let mut sorted: Vec<Real> = values.iter().copied().filter(|v| !v.is_nan()).collect();
    if sorted.is_empty() {
        return None;
    }
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p = p.clamp(0.0, 1.0);
    let n = sorted.len();
    if n == 1 {
        return Some(sorted[0]);
    }
    let h = (n as Real - 1.0) * p;
    let lo = h.floor() as usize;
    let hi = (lo + 1).min(n - 1);
    let frac = h - lo as Real;
    Some(sorted[lo] + (sorted[hi] - sorted[lo]) * frac)
}

/// Sample standard deviation (Bessel-corrected) of `values`, or `None` for
/// fewer than two finite samples.
pub fn std_dev(values: &[Real]) -> Option<Real> {
    let finite: Vec<Real> = values.iter().copied().filter(|v| v.is_finite()).collect();
    if finite.len() < 2 {
        return None;
    }
    let n = finite.len() as Real;
    let mean = finite.iter().sum::<Real>() / n;
    let var = finite.iter().map(|v| (v - mean).powi(2)).sum::<Real>() / (n - 1.0);
    Some(var.sqrt())
}

#[cfg(all(test, feature = "montecarlo"))]
mod tests {
    use super::*;

    #[test]
    fn same_seed_gives_identical_indices() {
        let a = resample_indices(
            50,
            ResampleScheme::Stationary { mean_block: 8.0 },
            &mut rng_from_seed(42),
        );
        let b = resample_indices(
            50,
            ResampleScheme::Stationary { mean_block: 8.0 },
            &mut rng_from_seed(42),
        );
        assert_eq!(a, b, "a fixed seed must reproduce the resample");
    }

    #[test]
    fn different_seed_diverges() {
        let a = resample_indices(50, ResampleScheme::Iid, &mut rng_from_seed(1));
        let b = resample_indices(50, ResampleScheme::Iid, &mut rng_from_seed(2));
        assert_ne!(a, b);
    }

    #[test]
    fn every_scheme_stays_in_range_and_full_length() {
        for scheme in [
            ResampleScheme::Iid,
            ResampleScheme::MovingBlock { block: 7 },
            ResampleScheme::Stationary { mean_block: 10.0 },
        ] {
            let idx = resample_indices(123, scheme, &mut rng_from_seed(7));
            assert_eq!(idx.len(), 123);
            assert!(idx.iter().all(|&i| i < 123));
        }
    }

    #[test]
    fn empty_series_resamples_to_empty() {
        assert!(resample_indices(0, ResampleScheme::Iid, &mut rng_from_seed(0)).is_empty());
    }

    #[test]
    fn moving_block_advances_by_one_within_a_block() {
        // With block == n the whole output is one contiguous circular run, so
        // consecutive indices always step by +1 (mod n).
        let n = 20;
        let idx = resample_indices(
            n,
            ResampleScheme::MovingBlock { block: n },
            &mut rng_from_seed(3),
        );
        for w in idx.windows(2) {
            assert_eq!(w[1], (w[0] + 1) % n);
        }
    }

    #[test]
    fn stationary_mean_block_length_is_about_right() {
        // The expected run of +1 steps before a jump is `mean_block`; measure
        // the realized mean block length over a long draw and check it lands in
        // a loose band around the target.
        let n = 5000;
        let mean_block = 10.0;
        let idx = resample_indices(
            n,
            ResampleScheme::Stationary { mean_block },
            &mut rng_from_seed(99),
        );
        let mut blocks = 1usize;
        for w in idx.windows(2) {
            if w[1] != (w[0] + 1) % n {
                blocks += 1;
            }
        }
        let realized = n as Real / blocks as Real;
        assert!(
            (realized - mean_block).abs() < 3.0,
            "realized mean block {realized} far from target {mean_block}"
        );
    }

    #[test]
    fn percentile_interpolates() {
        let v = [1.0, 2.0, 3.0, 4.0];
        assert_eq!(percentile(&v, 0.0), Some(1.0));
        assert_eq!(percentile(&v, 1.0), Some(4.0));
        assert_eq!(percentile(&v, 0.5), Some(2.5));
    }
}
