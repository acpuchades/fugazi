//! Minimal ANSI styling for the console logs — no color dependency.
//!
//! Colour is enabled only when stdout is a terminal and `NO_COLOR` is unset
//! (see <https://no-color.org>). When it is disabled every helper returns the
//! plain string unchanged, so redirected/piped output stays byte-identical to a
//! no-color build. The probe runs once and is cached.
//!
//! Helpers take and return owned `String`s so a styled fragment drops straight
//! into a `format!`. Pad to a fixed width *before* wrapping in a style — the
//! escape codes are invisible bytes that would otherwise throw off `{:<n}`.

use std::io::IsTerminal;
use std::sync::OnceLock;

/// Whether to emit escape codes — `NO_COLOR` unset and stdout is a terminal.
fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal())
}

/// Wrap `s` in the SGR `code` (e.g. `"1"`, `"32"`), or return it unchanged when
/// color is disabled.
fn paint(code: &str, s: &str) -> String {
    if enabled() {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

pub fn bold(s: &str) -> String {
    paint("1", s)
}

pub fn dim(s: &str) -> String {
    paint("2", s)
}

pub fn green(s: &str) -> String {
    paint("32", s)
}

pub fn red(s: &str) -> String {
    paint("31", s)
}

pub fn yellow(s: &str) -> String {
    paint("33", s)
}

/// The banner every subcommand prints at the top of its console output.
/// Line 1 is the constant tool identity (the same for any subcommand);
/// line 2 names the active command and what it does.
pub fn print_header(command: &str, description: &str) {
    println!(
        "{} · {}",
        bold(&format!(
            "{} {}",
            env!("CARGO_PKG_NAME"),
            env!("CARGO_PKG_VERSION")
        )),
        dim(env!("CARGO_PKG_REPOSITORY"))
    );
    println!("{}", dim(&format!("{command} · {description}")));
    println!();
}

/// A section title: `bold` at column 0 with no trailing decoration. Used by
/// every command's body blocks (`inputs`, `result`, `metrics`, `best`, …) so
/// section boundaries are visually consistent across subcommands.
pub fn print_section(title: &str) {
    println!("{}", bold(title));
}

/// One `label value` row inside a section, indented two spaces with a fixed
/// dim-styled label column. Callers should pass short labels (≤ the pad width);
/// longer labels get one padding space appended so alignment degrades
/// gracefully rather than jamming.
pub fn print_field(label: &str, value: &str, pad: usize) {
    let padded = if label.len() < pad {
        format!("{label:<pad$}")
    } else {
        format!("{label} ")
    };
    println!("  {}{value}", dim(&padded));
}

/// A hangover continuation under a [`print_field`] value — indented so the
/// text lands under the value column (2 leading spaces + `pad` label column).
pub fn print_field_continuation(text: &str, pad: usize) {
    let indent = 2 + pad;
    println!("{:indent$}{text}", "", indent = indent);
}

/// A top-level warning line printed at column 0, above the section it would
/// otherwise sit inside as a masquerading field. The `warn` prefix is yellow;
/// the message body is plain. Pass every warning through [`print_warns`]
/// instead of calling this directly so the trailing blank line is emitted only
/// when at least one warning fires.
pub fn print_warn(msg: &str) {
    println!("{} {msg}", yellow("warn"));
}

/// Emit each warning as a column-0 line and a trailing blank line separating
/// the warnings from the following section. A no-op when the slice is empty
/// so callers can hand off a lazily-built list without a guard.
pub fn print_warns(warns: &[String]) {
    if warns.is_empty() {
        return;
    }
    for w in warns {
        print_warn(w);
    }
    println!();
}

// ---------------------------------------------------------------------------
// Shared result-block formatting
// ---------------------------------------------------------------------------
//
// `run` and `optimize` both close with a result block, and both grew their own
// copy of these. `format_utc` in particular was ~25 lines of civil-from-days
// arithmetic duplicated byte-for-byte, under a comment declining to lift it —
// which is exactly the kind of calendar code where a fix reaching one copy and
// not the other is invisible until someone reads a wrong date.

/// A result-block field at the standard 9-character label column.
pub fn field(label: &str, value: &str) {
    print_field(label, value, 9);
}

/// A hangover line under a [`field`], indented to the value column.
pub fn field_continuation(text: &str) {
    print_field_continuation(text, 9);
}

/// Human-readable elapsed time: milliseconds under a second, seconds under a
/// minute, `MMm SSs` beyond.
pub fn format_elapsed(d: std::time::Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 1.0 {
        format!("{} ms", d.as_millis())
    } else if secs < 60.0 {
        format!("{secs:.2} s")
    } else {
        format!("{}m {:02}s", d.as_secs() / 60, d.as_secs() % 60)
    }
}

/// Format a [`SystemTime`](std::time::SystemTime) as `YYYY-MM-DD HH:MM:SS UTC`,
/// without pulling in a date library (Howard Hinnant's civil-from-days).
pub fn format_utc(t: std::time::SystemTime) -> String {
    let secs = t
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (hour, min, sec) = (rem / 3_600, (rem % 3_600) / 60, rem % 60);

    let z = days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);

    format!("{year:04}-{month:02}-{day:02} {hour:02}:{min:02}:{sec:02} UTC")
}

/// The console warning list shared by the `run` and `optimize` result blocks:
/// non-numeric overlay columns that were dropped, then the frictionless notice.
///
/// `what` names the thing the missing cost model affects — `"results"` for a
/// single run, `"grid results"` for a sweep. That one word was the *only*
/// difference between the two copies this replaces.
pub fn collect_warnings(skipped: &[String], no_cost: bool, what: &str) -> Vec<String> {
    let mut w = Vec::new();
    if !skipped.is_empty() {
        w.push(format!(
            "skipped non-numeric overlay column{}: {} — not accessible via `!get`",
            if skipped.len() == 1 { "" } else { "s" },
            skipped.join(", "),
        ));
    }
    if no_cost {
        w.push(format!(
            "no cost model set — commission, spread, and slippage are zero; \
             {what} are frictionless"
        ));
    }
    w
}
