//! Shared rayon thread-pool constructor for parallel commands.
//!
//! `optimize` fans out grid points across a rayon pool sized by `-j/--jobs`.
//! A nonzero `N` sizes the pool; `None` falls back to rayon's default (one
//! worker per logical CPU).

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
