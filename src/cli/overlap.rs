//! The console half of the universe-overlap diagnostic.
//!
//! The measurement itself — [`Overlap`], [`measure`], [`measure_universe`] —
//! lives in the library ([`fugazi::overlap`]), because two of the crate's
//! three consumers (Python, embedders) build their own snapshot streams and
//! need the same fragmented-universe check the CLI runs. This module keeps
//! only what is a property of a *terminal*: the stderr warning and its
//! formatting. See the library module's docs for what fragmentation is and
//! why exact-timestamp grouping makes it silent.

use crate::style;
pub use fugazi::overlap::{Overlap, measure, measure_universe};

/// What a fragmented universe costs a *run* — the clause
/// [`warn_if_fragmented`] appends. `get` phrases the same finding as a property
/// of the dataset it is writing; here it is a property of the run in front of
/// the user.
pub const RUN_CONSEQUENCE: &str = "this run's strategy sees only the symbols present on the \
     bar (a cross-sectional selection ranks that many, not the declared universe), and a \
     `!pick` across the boundary reads `None`.";

/// Report a universe no snapshot ever holds in full.
///
/// Fires only on the unambiguous case — two or more symbols, and `widest <
/// total`. A universe that *does* meet somewhere but not on every bar is
/// ordinary: listing gaps, holidays and half-days all produce partial
/// snapshots, and warning about them would bury the case that matters.
///
/// `at_label` renders the widest snapshot's key (the caller owns the format;
/// `get` has millis to turn into a stamp, `run` has the label verbatim), and
/// `consequence` completes the sentence "…so <consequence>" with what this
/// costs *that* command.
///
/// Goes to stderr regardless of `--quiet`, which governs a command's success
/// summary rather than its correctness warnings.
pub fn warn_if_fragmented<K>(o: &Overlap<K>, at_label: Option<&str>, consequence: &str) {
    if !o.is_fragmented() {
        return;
    }
    eprintln!(
        "  {} at most {} of {} symbols ever share a bar — no snapshot here holds them all. \
         Bars group into snapshots by exact timestamp, so series stamped at different session \
         times never co-occur: {consequence}",
        style::yellow("warn"),
        o.widest,
        o.total,
    );
    // Aligned under the message body: two spaces of indent plus `warn `.
    let indent = "       ";
    if let Some(label) = at_label {
        eprintln!(
            "{indent}widest snapshot: {} ({label})",
            format_symbol_list(&o.widest_symbols),
        );
    }
    if !o.isolated.is_empty() {
        eprintln!(
            "{indent}never sharing a bar with any other symbol: {}",
            format_symbol_list(&o.isolated),
        );
    }
    eprintln!(
        "{indent}{} snapshot(s), {} holding a single symbol",
        o.snapshots, o.singletons,
    );
}

/// Render a symbol list for the console, capped so a wide universe doesn't
/// push the line above it off the screen.
pub fn format_symbol_list(syms: &[String]) -> String {
    const MAX: usize = 8;
    if syms.len() <= MAX {
        return syms.join(", ");
    }
    format!("{}, … (+{} more)", syms[..MAX].join(", "), syms.len() - MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_symbol_list_is_capped() {
        let syms: Vec<String> = (0..11).map(|i| format!("S{i}")).collect();
        assert_eq!(
            format_symbol_list(&syms),
            "S0, S1, S2, S3, S4, S5, S6, S7, … (+3 more)",
        );
        assert_eq!(format_symbol_list(&syms[..2]), "S0, S1");
    }
}
