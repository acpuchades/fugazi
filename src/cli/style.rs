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
