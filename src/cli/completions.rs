//! `fugazi completions <shell>` — emit a shell-completion script.
//!
//! Uses `clap_complete` for subcommand/flag completion, then post-processes the
//! output to teach the shell about the CLI's `@file` convention: the
//! `STRATEGY` positional and the `--series` / `--params` values all accept
//! `@path` to load from a file, and the shell has no way to know that on its
//! own — it treats `@candles.csv` as a filename literally beginning with `@`
//! and finds nothing on disk. The patch peels the `@` (and any leading
//! `key=value,` segments, since `--series`/`--params` are `,`-separated) before
//! delegating to the shell's built-in file completion.
//!
//! Currently the `@`-peel patch is applied only to the zsh script (the shell
//! whose completion machinery is easiest to extend inline); bash / fish /
//! elvish / powershell get the base clap-generated script.

use std::io;

use anyhow::Result;
use clap::CommandFactory;
use clap_complete::Shell;

use crate::Cli;

pub fn run(shell: Shell) -> Result<()> {
    let mut cmd = Cli::command();
    let name = cmd.get_name().to_string();
    let mut buf: Vec<u8> = Vec::new();
    clap_complete::generate(shell, &mut cmd, &name, &mut buf);
    let script = String::from_utf8(buf).expect("clap_complete emits UTF-8");
    let script = match shell {
        Shell::Zsh => patch_zsh(&script),
        _ => script,
    };
    use io::Write;
    io::stdout().write_all(script.as_bytes())?;
    Ok(())
}

/// Rewrite the zsh script so the `@file` values (`STRATEGY`, `--series`,
/// `--params`) complete filenames after the `@`.
fn patch_zsh(script: &str) -> String {
    const HELPER: &str = "\
# Value completion for arguments that accept fugazi's `@file` convention.
# Peels any leading `key=value,` / `@file,` segments (--series and --params are
# `,`-separated), then, if what's left starts with `@`, peels that and delegates
# to zsh's file completion so `@can<TAB>` finds `candles.csv`.
_fugazi_at_file() {
    compset -P '*,'
    if [[ $PREFIX == @* ]]; then
        compset -P '@'
        _files
    fi
}

";
    // clap emits each value slot as `:LABEL:_default` (or `:_default` when the
    // value's help text supplies the label inline, as for our `STRATEGY`
    // positional whose description ends `inline YAML`). Rewrite those three
    // specific slots — leaving the other `_default` uses (`CASH`, `N`, …)
    // untouched.
    let body = script
        .replace(":SERIES:_default", ":SERIES:_fugazi_at_file")
        .replace(":SPEC:_default", ":SPEC:_fugazi_at_file")
        .replace("inline YAML:_default", "inline YAML:_fugazi_at_file");
    format!("{HELPER}{body}")
}
