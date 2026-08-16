//! Guards that every YAML in `examples/` still parses.
//!
//! `tests/examples_compile.rs` protects the Rust examples; the YAML ones had no
//! equivalent, even though they are the surface most exposed to drift — a
//! renamed tag or a changed field breaks a document without touching a line of
//! Rust. These are the files the README and `doc/CLI.md` tell a new user to run
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
