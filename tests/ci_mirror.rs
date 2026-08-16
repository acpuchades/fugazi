//! `scripts/ci-local.sh` must run what `.github/workflows/ci.yml` runs.
//!
//! The script exists because three CI checks fire nowhere else — the rustdoc
//! lints (only under `RUSTDOCFLAGS=-D warnings`), clippy over `python/src`
//! (every other clippy invocation is scoped `-p fugazi`), and the feature
//! matrix (`live` is compiled in no other job). A local gate that has drifted
//! from CI is worse than none: it reports green and the push goes red, which is
//! exactly the failure this file exists to prevent.
//!
//! The two are plain text with no compile-time link, so this is the same
//! hand-maintained-mirror treatment `tests/hand_maintained_mirrors.rs` gives
//! `NodeSpecRaw`: extract every command CI runs, and assert the script runs it
//! too. Textual and coarse, because the drift is textual — a step added to the
//! workflow and not to the script.
//!
//! It deliberately does **not** check the reverse direction. The script may run
//! more than CI does (a stricter local gate is fine); it may not run less.

const WORKFLOW: &str = include_str!("../.github/workflows/ci.yml");
const SCRIPT: &str = include_str!("../scripts/ci-local.sh");

/// Collapse runs of whitespace so a command split across the two files by
/// formatting still compares equal.
fn normalize(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Every `cargo …` invocation in the workflow, with the feature-matrix
/// placeholders expanded into one command per row.
fn workflow_cargo_commands() -> Vec<String> {
    let rows = matrix_rows();
    let mut out = Vec::new();
    for line in WORKFLOW.lines() {
        let line = line.trim();
        // `run: cargo …` and bare `cargo …` inside a block scalar both count.
        let cmd = line.strip_prefix("run: ").unwrap_or(line);
        if !cmd.starts_with("cargo ") {
            continue;
        }
        if cmd.contains("${{") {
            for (features, targets) in &rows {
                out.push(normalize(
                    &cmd.replace("${{ matrix.features }}", features)
                        .replace("${{ matrix.targets || '--lib' }}", targets),
                ));
            }
        } else {
            out.push(normalize(cmd));
        }
    }
    out.sort();
    out.dedup();
    out
}

/// The `features:` / `targets:` pairs of the matrix `include:` list.
fn matrix_rows() -> Vec<(String, String)> {
    let mut rows: Vec<(String, String)> = Vec::new();
    for line in WORKFLOW.lines() {
        let t = line.trim();
        if let Some(v) = t.strip_prefix("- features: ") {
            rows.push((v.trim_matches('"').to_string(), "--lib".to_string()));
        } else if let Some(v) = t.strip_prefix("targets: ")
            && let Some(last) = rows.last_mut()
        {
            last.1 = v.trim_matches('"').to_string();
        }
    }
    assert!(!rows.is_empty(), "no feature-matrix rows found in ci.yml");
    rows
}

#[test]
fn the_local_script_runs_every_cargo_command_ci_runs() {
    // The script wraps each invocation in a `run <label>` helper and splits it
    // across lines, so both sides are whitespace-normalized before comparing.
    let script = normalize(SCRIPT);
    let missing: Vec<String> = workflow_cargo_commands()
        .into_iter()
        .filter(|cmd| !script.contains(cmd.as_str()))
        .collect();

    assert!(
        missing.is_empty(),
        "`.github/workflows/ci.yml` runs commands `scripts/ci-local.sh` does not:\n{}\n\n\
         Add them to the script (and keep the order), or the local gate reports \
         green on a tree CI will reject.",
        missing
            .iter()
            .map(|c| format!("  {c}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn the_local_script_carries_the_env_vars_ci_sets() {
    // Both are load-bearing and both are invisible when missing: without
    // FUGAZI_REQUIRE_FIXTURES a stale fixture *skips* (a skip is
    // indistinguishable from a pass), and without RUSTDOCFLAGS the doc lints
    // simply don't run.
    for var in ["FUGAZI_REQUIRE_FIXTURES", "RUSTDOCFLAGS"] {
        assert!(
            WORKFLOW.contains(var),
            "ci.yml no longer sets {var} — update this guard"
        );
        assert!(
            SCRIPT.contains(var),
            "scripts/ci-local.sh does not set {var}, which ci.yml does"
        );
    }
}

#[test]
fn the_local_script_runs_the_python_suite() {
    assert!(
        WORKFLOW.contains("pytest"),
        "ci.yml no longer runs pytest — update this guard"
    );
    assert!(
        SCRIPT.contains("pytest"),
        "scripts/ci-local.sh must run the Python suite; ci.yml does"
    );
}

#[test]
fn every_feature_matrix_row_is_in_the_local_script() {
    let script = normalize(SCRIPT);
    for (features, _) in matrix_rows() {
        assert!(
            script.contains(&normalize(&features)),
            "feature-matrix row `{features}` is in ci.yml but not in scripts/ci-local.sh"
        );
    }
}
