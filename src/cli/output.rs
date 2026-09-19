//! Shared file-output plumbing for the CLI's writers.
//!
//! `optimize.rs` alone open-coded the parent-directory preamble eight times
//! and the CSV-writer constructor six; each new writer copied a neighbour,
//! which is how the copies drift. The writers themselves stay with their
//! subcommand — this module holds only the lines they all share.

use std::path::Path;

use anyhow::{Context, Result};

/// Create `path`'s parent directory if it names one — the preamble every
/// file-writing subcommand runs so `-o out/deep/file.csv` works on a fresh
/// tree. A bare filename (empty parent) needs nothing and gets nothing.
pub(crate) fn ensure_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating output dir `{}`", parent.display()))?;
    }
    Ok(())
}

/// A `,`-delimited CSV writer at `path`, parent directory ensured first.
pub(crate) fn writer(path: &Path) -> Result<csv::Writer<std::fs::File>> {
    ensure_parent(path)?;
    csv::WriterBuilder::new()
        .delimiter(b',')
        .from_path(path)
        .with_context(|| format!("creating `{}`", path.display()))
}
