//! Guards that every YAML in `examples/` still parses.
//!
//! `tests/examples_compile.rs` protects the Rust examples; the YAML ones had no
//! equivalent, even though they are the surface most exposed to drift — a
//! renamed tag or a changed field breaks a document without touching a line of
//! Rust. These are the files the README and `docs/CLI.md` tell a new user to run
//! first, so a broken one is the worst possible first impression.
//!
//! Each is checked with the `fugazi check` subcommand that matches its kind:
//! `check strategy` for the four strategy documents, `check costs` for the two
//! fee schedules. `examples/params.yml` is a plain `NAME: value` mapping, not a
//! spec — it is exercised as the `--params` input to the document that reads it.

mod common;

use common::cli::Cmd;

/// Repo-relative path, as `@file` for the spec-loading arguments.
fn at(rel: &str) -> String {
    format!("@{}/{rel}", env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn every_strategy_example_parses() {
    // The `kind:` prefix is part of the argument, and each document's own
    // header comment shows the invocation — these match them exactly, so the
    // test fails if the documented command stops working.
    //
    // `strategy.params.yml` is deliberately absent: its `!param` placeholders
    // are exercised with a real params file in the test below.
    for arg in [
        at("examples/strategy.yml"),
        format!("pairs:{}", at("examples/pairs.yml")),
        format!("basket:{}", at("examples/basket.yml")),
    ] {
        Cmd::new("check").arg("strategy").arg(&arg).ok();
    }
}

#[test]
fn the_parameterised_example_parses_with_its_params_file() {
    // The pair is only meaningful together: `params.yml` exists to resolve
    // `strategy.params.yml`'s placeholders, and neither file is checked by
    // anything else. This is the exact invocation the document's own header
    // comment tells a user to run.
    Cmd::new("check")
        .arg("strategy")
        .arg(&at("examples/strategy.params.yml"))
        .args(&["--params", &at("examples/params.yml")])
        .ok();
}

#[test]
fn the_parameterised_example_also_parses_with_placeholders_unset() {
    // `check` holds an unset required `!param` as a typed hole rather than
    // failing — that is the whole point of the `undefined` machinery. Pinning
    // it here means a change to that machinery can't quietly start rejecting
    // the documented "validate before you have values" workflow.
    Cmd::new("check")
        .arg("strategy")
        .arg(&at("examples/strategy.params.yml"))
        .ok();
}

#[test]
fn every_cost_example_parses() {
    for doc in ["ibkr.yml", "binance.yml"] {
        Cmd::new("check")
            .arg("costs")
            .arg(&at(&format!("examples/{doc}")))
            .ok();
    }
}

/// The `fugazi run …` invocation each example documents in its own header
/// must actually work.
///
/// Parsing tells you a document is well-formed; it doesn't tell you the command
/// the file tells a user to type still exists. Three examples documented
/// `fugazi run --strategy @file` — the strategy is positional and `--strategy`
/// has never been a flag, so all three failed at argument parsing, before
/// anything was even loaded. That is the first command a new user runs.
#[test]
fn the_invocation_each_example_documents_still_works() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
    let mut checked = 0;

    for entry in std::fs::read_dir(&dir).expect("read examples/") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("yml") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("read example");

        // The header comments are the documentation. A command starts on a line
        // whose comment body begins `fugazi ` and continues across every line
        // ending in a backslash — which is exactly how these files are written,
        // and far more reliable than guessing where prose resumes.
        let mut commands: Vec<String> = Vec::new();
        let mut lines = text
            .lines()
            .take_while(|l| l.starts_with('#') || l.trim().is_empty())
            .map(|l| l.trim_start_matches('#').trim());
        while let Some(line) = lines.next() {
            let Some(first) = line.strip_prefix("fugazi ") else {
                continue;
            };
            let mut cmd = first.trim_end_matches('\\').trim().to_string();
            let mut continued = line.ends_with('\\');
            while continued {
                let Some(next) = lines.next() else { break };
                continued = next.ends_with('\\');
                cmd.push(' ');
                cmd.push_str(next.trim_end_matches('\\').trim());
            }
            commands.push(cmd);
        }

        for cmd in &commands {
            let argv: Vec<&str> = cmd.split_whitespace().collect();
            if argv.first().is_none_or(|c| *c != "run") {
                continue;
            }
            // Only the flags matter for this check; point the output at scratch
            // and make relative `@paths` resolve from the repo root.
            let out = common::cli::unique_path("fugazi_example_doc");
            let _ = std::fs::remove_dir_all(&out);
            let mut c = Cmd::new("run");
            let mut rest = argv[1..].iter().peekable();
            while let Some(a) = rest.next() {
                // Drop the documented output dir (flag *and* value) — this test
                // writes to scratch.
                if *a == "--output-dir" || *a == "-o" {
                    rest.next();
                    continue;
                }
                c = c.arg(&a.replace('@', &format!("@{}/", env!("CARGO_MANIFEST_DIR"))));
            }
            let outcome = c
                .args(&["--output-dir", &out.to_string_lossy()])
                .arg("--quiet")
                .run();
            let name = path.file_name().unwrap().to_string_lossy();

            // Some examples cite placeholder data files (`@btc.csv`) to show the
            // shape of a two-series invocation. Those can't run here — but the
            // failure that motivated this test was clap rejecting the *flags*,
            // which happens before any file is opened. So every documented
            // invocation is held to that, and the ones whose data actually
            // exists are additionally required to succeed.
            for bad in ["unexpected argument", "a value is required", "invalid value"] {
                assert!(
                    !outcome.stderr.contains(bad),
                    "{name}'s documented invocation is not valid CLI:\n  fugazi {}\n{}",
                    argv.join(" "),
                    outcome.stderr,
                );
            }
            let runnable = argv.iter().all(|a| {
                a.strip_prefix('@').is_none_or(|f| {
                    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(f).exists()
                })
            });
            if runnable {
                assert!(
                    outcome.status.success(),
                    "{name}'s documented invocation fails:\n  fugazi {}\n{}",
                    argv.join(" "),
                    outcome.stderr,
                );
            }
            checked += 1;
        }
    }

    assert!(checked >= 2, "expected to find documented `fugazi run` invocations, found {checked}");
}

/// Every `examples/*.yml` is covered by one of the tests above.
///
/// Without this, adding an example silently gets no coverage — the same
/// opt-in-battery failure mode the warm-up and reference suites have.
#[test]
fn no_example_yaml_is_left_unchecked() {
    let covered = [
        "strategy.yml",
        "strategy.params.yml",
        "params.yml",
        "pairs.yml",
        "basket.yml",
        "ibkr.yml",
        "binance.yml",
    ];

    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
    let mut found: Vec<String> = std::fs::read_dir(&dir)
        .expect("read examples/")
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".yml") || n.ends_with(".yaml"))
        .collect();
    found.sort();

    let uncovered: Vec<&String> = found.iter().filter(|n| !covered.contains(&n.as_str())).collect();
    assert!(
        uncovered.is_empty(),
        "these examples/ YAMLs have no check above: {uncovered:?}",
    );
}
