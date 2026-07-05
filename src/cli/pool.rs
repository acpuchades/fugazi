//! Shared rayon thread-pool constructor for parallel commands.
//!
//! `optimize` and `run --multiple`/batch-mode `run` both fan out work across
//! independent iterations (grid points, or `(symbol, freq)` groups) via
//! rayon. Both accept an optional `-j/--jobs N` — a nonzero `N` sizes the
//! pool, `None` falls back to rayon's default (one worker per logical CPU).
//! Keeping the constructor here keeps the two callers in lockstep.

use anyhow::{Context, Result};

/// Build a rayon [`rayon::ThreadPool`] with an explicit worker count when
/// `jobs` is `Some`, else rayon's default (one worker per logical CPU).
pub fn build_pool(jobs: Option<usize>) -> Result<rayon::ThreadPool> {
    let mut builder = rayon::ThreadPoolBuilder::new();
    if let Some(n) = jobs {
        builder = builder.num_threads(n);
    }
    builder
        .build()
        .context("building the rayon thread pool for --jobs")
}
