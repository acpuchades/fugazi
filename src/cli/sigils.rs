//! CLI-managed template variables for `run`'s batch mode.
//!
//! When a run iterates over several `(symbol, freq)` series (see
//! [`crate::batch`]), users need a way to feed the current iteration's
//! symbol and freq into their `--params` values and their `--output-dir`
//! path without hardcoding the strategy YAML. `%SYMBOL` and `%FREQ` are the
//! two CLI-reserved template variables the CLI expands per iteration.
//!
//! * `%SYMBOL` — expands to the iteration's symbol.
//! * `%FREQ` — expands to the iteration's effective bar frequency (the
//!   canonical code, e.g. `1d`, `4h`), or an empty string when the freq is
//!   unknown (auto-detection failed and no `-f/--frequency` was given).
//!
//! Expansion is applied in exactly two places: the value column of
//! `--params` (see [`crate::params`]) and the `--output-dir` path (see
//! [`crate::batch`]). The scope prefixes on `--costs`,
//! `-f/--frequency`, and `--bars-per-year` do their own matching and are
//! not substituted through — that would double up on the matcher's work.
//!
//! Both sigil values are [`normalize`]d before substitution: any
//! path-hostile character (`/`, `\`, `:`, `?`, `*`, `"`, `<`, `>`, `|`) is
//! replaced with `_`. That keeps values like `BTC/USDT` (Binance's spelling
//! of the same pair) from carving unintended subdirectories into
//! `--output-dir`. The `%SYMBOL` namespace is reserved: users may reference
//! `%SYMBOL`/`%FREQ` in string args but may not declare param names
//! starting with `%` (see [`crate::params::parse_term`], which enforces
//! this up front).

use std::path::{Path, PathBuf};

use crate::calendar::Frequency;

/// Replace `%SYMBOL` / `%FREQ` occurrences in `value` with their normalized
/// per-iteration substitutions. Unknown sigils (`%FOO`) are left as-is —
/// they're not this layer's concern, and the caller has already rejected
/// them at arg-parse time when they appear as *names* rather than values.
pub fn expand(value: &str, symbol: &str, freq: Option<Frequency>) -> String {
    let sym = normalize(symbol);
    let fq = freq.map(freq_code).map(|s| normalize(&s)).unwrap_or_default();
    value.replace("%SYMBOL", &sym).replace("%FREQ", &fq)
}

/// Path-friendly form of [`expand`]: normalize the string form of `path`,
/// substitute the two sigils, and hand back a `PathBuf`. Used for
/// `--output-dir` per-iteration expansion.
pub fn expand_path(path: &Path, symbol: &str, freq: Option<Frequency>) -> PathBuf {
    PathBuf::from(expand(&path.to_string_lossy(), symbol, freq))
}

/// Replace path-hostile characters with `_`. Called on both sigil values
/// (so a symbol like `BTC/USDT` doesn't carve a directory) before
/// substitution.
pub fn normalize(v: &str) -> String {
    v.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '?' | '*' | '"' | '<' | '>' | '|' => '_',
            other => other,
        })
        .collect()
}

/// Canonical string form of a [`Frequency`], matching the `-f/--frequency`
/// grammar (`5m`, `1h`, `1d`, `1w`, `1M`). Used for `%FREQ` substitution and
/// for output-directory naming.
pub fn freq_code(f: Frequency) -> String {
    match f {
        Frequency::Minute(n) => format!("{n}m"),
        Frequency::Hour(n) => format!("{n}h"),
        Frequency::Day(n) => format!("{n}d"),
        Frequency::Week(n) => format!("{n}w"),
        Frequency::Month(n) => format!("{n}M"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_substitutes_both_sigils() {
        let out = expand("out/%SYMBOL/%FREQ/trades", "BTC", Some(Frequency::Hour(1)));
        assert_eq!(out, "out/BTC/1h/trades");
    }

    #[test]
    fn expand_normalizes_slash_in_symbol() {
        // Binance spelling `BTC/USDT` must not become a subdirectory.
        let out = expand("out/%SYMBOL/data", "BTC/USDT", Some(Frequency::Day(1)));
        assert_eq!(out, "out/BTC_USDT/data");
    }

    #[test]
    fn expand_freq_empties_when_unknown() {
        // Detection failed and no `-f` — `%FREQ` collapses to empty. The
        // caller (batch driver) decides whether the resulting path is usable.
        assert_eq!(expand("out/%SYMBOL/%FREQ", "BTC", None), "out/BTC/");
    }

    #[test]
    fn expand_leaves_unknown_sigils_intact() {
        // Only the two reserved sigils are substituted; anything else passes
        // through verbatim. Users hit `params::parse_term`'s guard before
        // reaching this function anyway when they try to *declare* one.
        assert_eq!(
            expand("out/%FOO/%SYMBOL", "BTC", Some(Frequency::Day(1))),
            "out/%FOO/BTC"
        );
    }

    #[test]
    fn expand_path_builds_a_pathbuf() {
        let base = PathBuf::from("out/%SYMBOL/%FREQ");
        let expanded = expand_path(&base, "AAPL", Some(Frequency::Day(1)));
        assert_eq!(expanded, PathBuf::from("out/AAPL/1d"));
    }

    #[test]
    fn normalize_replaces_all_hostile_chars() {
        assert_eq!(normalize("a/b\\c:d?e*f\"g<h>i|j"), "a_b_c_d_e_f_g_h_i_j");
    }

    #[test]
    fn freq_code_matches_from_str_roundtrip() {
        // The code we emit must parse back into the same Frequency (except
        // where users write non-canonical multipliers we never emit).
        use std::str::FromStr;
        for f in [
            Frequency::Minute(5),
            Frequency::Hour(4),
            Frequency::Day(1),
            Frequency::Week(1),
            Frequency::Month(1),
        ] {
            let code = freq_code(f);
            assert_eq!(Frequency::from_str(&code).unwrap(), f);
        }
    }
}
