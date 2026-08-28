//! End-to-end: `fugazi run --montecarlo` and the six `--mc-*` knobs.
//!
//! `tests/montecarlo.rs` covers the library layer thoroughly — the resampling
//! core, the CIs, the re-run empirical null, reproducibility. None of it goes
//! through the binary, and **every one of the seven flags appeared nowhere in
//! `tests/`**: `--montecarlo`, `--mc-permutations`, `--mc-scheme`, `--mc-block`,
//! `--mc-seed`, `--mc-null`, `--mc-ci`, `--mc-metrics`. So the whole
//! flag → `McConfig` → `metrics.yml` wiring was unpinned, which is exactly the
//! layer `docs/TESTING.md` says a CLI flag belongs to ("that a flag reaches the
//! resolver, that a column appears").
//!
//! The numbers themselves are not re-derived here — that is the library suite's
//! job. What is asserted is that each knob *reaches* the run and changes what it
//! is supposed to change.

mod common;

use common::cli::{Cmd, Outcome, at};

/// An MA-crossover over the example candles — the same document `tests/run.rs`
/// drives, so a failure here is about the Monte Carlo flags rather than the
/// strategy.
const STRATEGY: &str = "\
root: BTC
long:
  enter: !crosses_above { lhs: !sma { source: close, period: 2 }, rhs: !sma { source: close, period: 4 } }
  exit: !crosses_below { lhs: !sma { source: close, period: 2 }, rhs: !sma { source: close, period: 4 } }
";

/// A run with `--montecarlo` plus whatever extra flags, into a fresh dir.
/// Permutations are kept small — the re-run null drives a whole backtest per
/// permutation, and this file is about wiring, not about a tight interval.
fn run(name: &str, extra: &[&str]) -> Outcome {
    Cmd::new("run")
        .arg(STRATEGY)
        .series(&at("examples/candles.csv"))
        .output_dir(name)
        .args(&["--montecarlo", "--mc-permutations", "40", "-q"])
        .args(extra)
        .ok()
}

/// The `montecarlo:` block of a run's `metrics.yml`, as raw text.
fn block(out: &Outcome) -> String {
    let metrics = out.read("metrics.yml");
    let start = metrics
        .find("montecarlo:")
        .unwrap_or_else(|| panic!("no `montecarlo:` block in metrics.yml:\n{metrics}"));
    metrics[start..].to_string()
}

/// A `key: value` scalar from the block, by its indented key.
fn field(block: &str, key: &str) -> String {
    block
        .lines()
        .find_map(|l| l.trim().strip_prefix(&format!("{key}: ")))
        .unwrap_or_else(|| panic!("no `{key}` in the montecarlo block:\n{block}"))
        .to_string()
}

/// The `name:` of every analyzed metric, in order.
fn analyzed(block: &str) -> Vec<String> {
    block
        .lines()
        .filter_map(|l| l.trim().strip_prefix("- name: "))
        .map(str::to_string)
        .collect()
}

/// **The flag turns the block on, and its absence leaves it off.**
///
/// Both halves matter: a `montecarlo:` block written unconditionally would
/// satisfy every other test in this file.
#[test]
fn the_flag_adds_a_montecarlo_block_and_a_per_resample_csv() {
    let on = run("mc_on", &[]);
    let b = block(&on);
    assert_eq!(field(&b, "permutations"), "40");
    assert!(
        on.wrote("montecarlo.csv"),
        "the per-resample values must be written alongside the block"
    );
    assert!(
        on.rows("montecarlo.csv").len() >= 40,
        "expected at least one row per resample, got {}",
        on.rows("montecarlo.csv").len()
    );

    let off = Cmd::new("run")
        .arg(STRATEGY)
        .series(&at("examples/candles.csv"))
        .output_dir("mc_off")
        .arg("-q")
        .ok();
    assert!(
        !off.read("metrics.yml").contains("montecarlo:"),
        "the analysis is opt-in — a plain run must not carry the block"
    );
    assert!(
        !off.wrote("montecarlo.csv"),
        "a plain run must not write montecarlo.csv"
    );
}

/// **A seed reproduces, and a different seed does not.**
///
/// The headline promise of the feature, asserted through the binary: the whole
/// block is byte-identical across two invocations with the same seed.
#[test]
fn the_seed_is_what_makes_a_report_reproducible() {
    let a = block(&run("mc_seed_a", &["--mc-seed", "7"]));
    let b = block(&run("mc_seed_b", &["--mc-seed", "7"]));
    let c = block(&run("mc_seed_c", &["--mc-seed", "8"]));

    assert_eq!(a, b, "the same seed must reproduce the report exactly");
    assert_ne!(
        a, c,
        "a different seed must draw a different permutation set"
    );
    assert_eq!(field(&a, "seed"), "7");
    assert_eq!(field(&c, "seed"), "8");
}

/// Each of the three schemes reaches the run and is recorded, and `--mc-block`
/// is reflected in the two that read it.
#[test]
fn the_scheme_and_block_length_reach_the_resampler() {
    let iid = block(&run("mc_iid", &["--mc-scheme", "iid"]));
    let moving = block(&run(
        "mc_moving",
        &["--mc-scheme", "moving-block", "--mc-block", "4"],
    ));
    let stationary = block(&run(
        "mc_stationary",
        &["--mc-scheme", "stationary", "--mc-block", "4"],
    ));

    assert_eq!(field(&iid, "scheme"), "iid");
    assert!(
        field(&moving, "scheme").contains('4'),
        "moving-block must record the length it used: {}",
        field(&moving, "scheme")
    );
    assert!(
        field(&stationary, "scheme").contains('4'),
        "stationary must record its expected block length: {}",
        field(&stationary, "scheme")
    );
    // `--mc-block` is documented as ignored for `iid`, so the three must not
    // simply be the same report under three labels.
    assert_ne!(iid, moving);
    assert_ne!(moving, stationary);
}

/// `--mc-ci` widens the interval it names, and is recorded.
#[test]
fn the_confidence_level_reaches_the_interval() {
    let narrow = block(&run("mc_ci_narrow", &["--mc-ci", "0.5", "--mc-seed", "3"]));
    let wide = block(&run("mc_ci_wide", &["--mc-ci", "0.99", "--mc-seed", "3"]));

    assert_eq!(field(&narrow, "ci_level"), "0.5");
    assert_eq!(field(&wide, "ci_level"), "0.99");

    let bound =
        |b: &str, key: &str| -> f64 { field(b, key).parse().expect("a numeric interval bound") };
    let narrow_width = bound(&narrow, "ci_upper") - bound(&narrow, "ci_lower");
    let wide_width = bound(&wide, "ci_upper") - bound(&wide, "ci_lower");
    assert!(
        wide_width > narrow_width,
        "a 99% interval must be wider than a 50% one over the same resamples \
         ({wide_width} vs {narrow_width})"
    );
}

/// `--mc-null none` computes the intervals and *no* p-values; the default
/// (`rerun`) computes both.
#[test]
fn the_null_choice_decides_whether_p_values_are_computed() {
    let none = block(&run("mc_null_none", &["--mc-null", "none"]));
    let rerun = block(&run("mc_null_rerun", &["--mc-null", "rerun"]));

    assert!(
        !none.contains("p_value"),
        "`--mc-null none` must skip the empirical null entirely:\n{none}"
    );
    assert!(
        none.contains("ci_lower"),
        "…but must still report the bootstrap intervals:\n{none}"
    );
    assert!(
        rerun.contains("p_value_rerun"),
        "`rerun` is the p-value the default computes:\n{rerun}"
    );
}

/// `--mc-metrics` selects what is analyzed, and accepts both the short and the
/// dotted spelling of a name.
#[test]
fn mc_metrics_selects_what_is_analyzed() {
    let default = analyzed(&block(&run("mc_metrics_default", &[])));
    assert!(
        default.len() > 1,
        "the default is a headline *set*: {default:?}"
    );

    let chosen = analyzed(&block(&run(
        "mc_metrics_chosen",
        &["--mc-metrics", "sharpe,drawdown.max_pct"],
    )));
    assert_eq!(
        chosen,
        vec!["risk_adjusted.sharpe", "drawdown.max_pct"],
        "both the short and the dotted spelling must resolve, and nothing else \
         may be analyzed"
    );
}

/// A metric name nothing resolves to is bad input: refused with a diagnostic,
/// not silently dropped from the analysis.
#[test]
fn an_unknown_mc_metric_is_refused() {
    let out = Cmd::new("run")
        .arg(STRATEGY)
        .series(&at("examples/candles.csv"))
        .output_dir("mc_metrics_bad")
        .args(&["--montecarlo", "--mc-permutations", "5", "-q"])
        .args(&["--mc-metrics", "not_a_metric"])
        .fails();
    assert!(
        out.stderr.contains("not_a_metric"),
        "the diagnostic must name the metric it could not resolve: {}",
        out.stderr
    );
}

/// A confidence level outside `(0, 1)` is refused before the expensive run.
#[test]
fn a_confidence_level_outside_the_open_unit_interval_is_refused() {
    for bad in ["0", "1", "1.5"] {
        let out = Cmd::new("run")
            .arg(STRATEGY)
            .series(&at("examples/candles.csv"))
            .output_dir("mc_ci_bad")
            .args(&["--montecarlo", "--mc-permutations", "5", "-q"])
            .args(&["--mc-ci", bad])
            .fails();
        assert!(
            !out.stderr.is_empty(),
            "--mc-ci {bad} must be refused with a diagnostic"
        );
    }
}
