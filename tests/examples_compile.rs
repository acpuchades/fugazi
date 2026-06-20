//! Guards that every file in `examples/` compiles cleanly.
//!
//! A plain `cargo test` builds the library and the test targets but **not** the
//! examples, so a regression that breaks an example can go unnoticed until
//! someone runs `cargo run --example ...`. This test discovers each
//! `examples/*.rs` and asks cargo to build all example targets, failing if any
//! does not compile. Cargo's own diagnostics (inherited to this test's output)
//! pinpoint the offending file.

use std::path::Path;
use std::process::Command;

#[test]
fn all_examples_compile() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let examples_dir = Path::new(manifest_dir).join("examples");

    // Discover the example files so the test is meaningful even if the build
    // succeeds trivially (e.g. the directory were emptied).
    let mut names: Vec<String> = std::fs::read_dir(&examples_dir)
        .expect("read examples/ directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("rs"))
        .filter_map(|path| path.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .collect();
    names.sort();
    assert!(!names.is_empty(), "expected at least one example in examples/");

    // `--examples` compiles every example target in one pass. Use the same cargo
    // that is running this test (`$CARGO`); the outer invocation has released
    // the build lock by the time test binaries run, so the nested build is safe.
    let status = Command::new(env!("CARGO"))
        .args(["build", "--examples", "--quiet"])
        .current_dir(manifest_dir)
        .status()
        .expect("failed to invoke cargo build --examples");

    assert!(
        status.success(),
        "one or more examples failed to compile (examples found: {names:?})"
    );
}
