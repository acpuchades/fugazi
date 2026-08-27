//! An omitted `root:` — the default evaluation root and how it resolves.
//!
//! A single-asset document may leave `root:` out, in which case it reads as
//! `!pick { symbol: !param { key: SYMBOL }, freq: !param { key: FREQ } }`,
//! spliced in before substitution so both placeholders resolve out of
//! `--params`. Neither is required: an unset one drops its key rather than
//! erroring, and the CLI seeds `SYMBOL` from a single-series input so the
//! sole-atom root still yields a symbol to route orders through.
//!
//! The failure this guards against is the quiet one. A null left where a
//! symbol was expected still *parses* — `!pick`'s fields are `Option`s — but it
//! costs `RootSpec::as_pick` its answer and with it `Pick::rooted`'s sole-atom
//! fallback, and the run then reports a plausible, fully-metricked zero-fill
//! backtest instead of a message.

mod common;

use common::cli::{Cmd, scratch_file};

/// A crossover document with **no** `root:` — the whole point of the fixture.
const NO_ROOT: &str = "\
long:
  enter: !crosses_above { lhs: !sma { period: 3 }, rhs: !sma { period: 10 } }
  exit:  !crosses_below { lhs: !sma { period: 3 }, rhs: !sma { period: 10 } }
";

/// Oscillating closes, so a crossover document actually changes state.
fn csv(symbols: &[&str]) -> String {
    let mut out = String::from("time,symbol,open,high,low,close,volume\n");
    for (k, sym) in symbols.iter().enumerate() {
        for i in 0..200u32 {
            let px = 100.0 + k as f64 + 10.0 * ((i as f64) / 7.0).sin();
            out.push_str(&format!(
                "2024-01-01T{:02}:{:02}:00Z,{sym},{px:.4},{:.4},{:.4},{px:.4},1000\n",
                i / 60,
                i % 60,
                px + 1.0,
                px - 1.0,
            ));
        }
    }
    out
}

fn fills_of(out: &common::cli::Outcome) -> Vec<String> {
    out.rows("fills.csv")
}

#[test]
fn a_single_series_run_infers_the_symbol() {
    let (_s, series) = scratch_file("one.csv", &csv(&["BTCUSDT"]));
    let (_d, doc) = scratch_file("no_root.yml", NO_ROOT);
    let out = Cmd::new("run")
        .arg(&doc)
        .series(&series)
        .args(&["--crypto", "--quiet"])
        .output_dir("default_root_one")
        .ok();
    let fills = fills_of(&out);
    assert!(!fills.is_empty(), "an inferred root must actually trade");
    assert!(
        fills.iter().all(|r| r.contains("BTCUSDT")),
        "fills should route to the inferred symbol: {fills:?}"
    );
}

/// The identical document, pinned with `--params SYMBOL=`, must produce the
/// identical run — the inference is a default, not a different code path.
#[test]
fn the_inferred_symbol_matches_an_explicit_one() {
    let (_s, series) = scratch_file("one.csv", &csv(&["BTCUSDT"]));
    let (_d, doc) = scratch_file("no_root.yml", NO_ROOT);
    let inferred = Cmd::new("run")
        .arg(&doc)
        .series(&series)
        .args(&["--crypto", "--quiet"])
        .output_dir("default_root_inferred")
        .ok();
    let explicit = Cmd::new("run")
        .arg(&doc)
        .series(&series)
        .args(&["--crypto", "--quiet", "--params", "SYMBOL=BTCUSDT"])
        .output_dir("default_root_explicit")
        .ok();
    assert_eq!(inferred.read("fills.csv"), explicit.read("fills.csv"));
    assert_eq!(inferred.read("trades.csv"), explicit.read("trades.csv"));
}

/// …and so must the fully spelled-out `root:`, which is the claim that the
/// default really is *only* sugar.
#[test]
fn the_default_root_matches_a_written_one() {
    let (_s, series) = scratch_file("one.csv", &csv(&["BTCUSDT"]));
    let (_d, implicit) = scratch_file("no_root.yml", NO_ROOT);
    let (_e, explicit) = scratch_file("with_root.yml", &format!("root: BTCUSDT\n{NO_ROOT}"));
    let a = Cmd::new("run")
        .arg(&implicit)
        .series(&series)
        .args(&["--crypto", "--quiet"])
        .output_dir("default_root_implicit")
        .ok();
    let b = Cmd::new("run")
        .arg(&explicit)
        .series(&series)
        .args(&["--crypto", "--quiet"])
        .output_dir("default_root_written")
        .ok();
    assert_eq!(a.read("fills.csv"), b.read("fills.csv"));
    assert_eq!(a.read("metrics.yml"), b.read("metrics.yml"));
}

#[test]
fn a_multi_series_run_needs_the_symbol_named() {
    let (_s, series) = scratch_file("two.csv", &csv(&["BTCUSDT", "ETHUSDT"]));
    let (_d, doc) = scratch_file("no_root.yml", NO_ROOT);
    // Nothing to infer from two symbols — and the message has to say what to do.
    let out = Cmd::new("run")
        .arg(&doc)
        .series(&series)
        .args(&["--crypto", "--quiet"])
        .output_dir("default_root_two")
        .fails();
    assert!(
        out.stderr.contains("names no symbol") && out.stderr.contains("SYMBOL"),
        "stderr should name the missing root and the way out: {}",
        out.stderr
    );
    // Named, it trades the one it was told to.
    let out = Cmd::new("run")
        .arg(&doc)
        .series(&series)
        .args(&["--crypto", "--quiet", "--params", "SYMBOL=ETHUSDT"])
        .output_dir("default_root_two_named")
        .ok();
    let fills = fills_of(&out);
    assert!(!fills.is_empty());
    assert!(
        fills.iter().all(|r| r.contains("ETHUSDT")),
        "fills should route to the named symbol only: {fills:?}"
    );
}

/// The payoff: one document, no `root:`, swept across a panel.
#[test]
fn optimize_sweeps_symbol_through_the_default_root() {
    let (_s, series) = scratch_file("two.csv", &csv(&["BTCUSDT", "ETHUSDT"]));
    let (_d, doc) = scratch_file("no_root.yml", NO_ROOT);
    let out = Cmd::new("optimize")
        .arg(&doc)
        .series(&series)
        .args(&[
            "--crypto",
            "--quiet",
            "--grid",
            "SYMBOL=[\"BTCUSDT\",\"ETHUSDT\"]",
            "--output",
        ])
        .arg(
            common::cli::unique_path("default_root_grid.csv")
                .to_str()
                .expect("utf-8"),
        )
        .run();
    assert!(out.status.success(), "{}", out.stderr);
}

/// `FREQ` reaches the root's **declared cadence**, which sits one rung below
/// `-f/--frequency` in the resolution chain and above the input's own `freq`
/// column — so it is what a run annualizes by.
///
/// (It does *not* narrow a multi-cadence slice for a single-asset run: the
/// frame refuses a symbol carrying two cadences before any root is consulted,
/// and `-f SYMBOL:CODE` is the way to say which. See `cli::cadence`.)
#[test]
fn freq_declares_the_cadence_the_run_annualizes_by() {
    let mut text = String::from("time,symbol,freq,open,high,low,close,volume\n");
    for i in 0..200u32 {
        let px = 100.0 + 10.0 * ((i as f64) / 7.0).sin();
        text.push_str(&format!(
            "2024-01-01T{:02}:{:02}:00Z,BTCUSDT,1h,{px:.4},{:.4},{:.4},{px:.4},1000\n",
            i / 60,
            i % 60,
            px + 1.0,
            px - 1.0,
        ));
    }
    let (_s, series) = scratch_file("hourly.csv", &text);
    let (_d, doc) = scratch_file("no_root.yml", NO_ROOT);
    let hourly = Cmd::new("run")
        .arg(&doc)
        .series(&series)
        .args(&["--crypto", "--quiet"])
        .output_dir("default_root_freq_column")
        .ok();
    let declared = Cmd::new("run")
        .arg(&doc)
        .series(&series)
        .args(&["--crypto", "--quiet", "--params", "FREQ=1d"])
        .output_dir("default_root_freq_param")
        .ok();
    assert!(
        hourly.read("metrics.yml").contains("bars_per_year: 8760"),
        "the freq column should annualize hourly:\n{}",
        hourly.read("metrics.yml")
    );
    assert!(
        declared.read("metrics.yml").contains("bars_per_year: 365"),
        "`--params FREQ=` should outrank the freq column:\n{}",
        declared.read("metrics.yml")
    );
}

/// `check` has no data, so it cannot resolve a root that defers to `--series`.
/// It must report that as pending, not as a broken document — the build is
/// skipped for the same reason a `!get` document's is.
#[test]
fn check_reports_a_deferred_root_as_ok() {
    let (_d, doc) = scratch_file("no_root.yml", NO_ROOT);
    let out = Cmd::new("check").arg("strategy").arg(&doc).ok();
    assert!(
        out.stdout.contains("ok") && out.stdout.contains("--series"),
        "check should say the root resolves from the input: {}",
        out.stdout
    );
    // With SYMBOL supplied there is nothing deferred, and check names it.
    let out = Cmd::new("check")
        .arg("strategy")
        .arg(&doc)
        .args(&["--params", "SYMBOL=BTCUSDT"])
        .ok();
    assert!(out.stdout.contains("root BTCUSDT"), "{}", out.stdout);
}

/// An unset **required** `!param` in the root is a missing *value*, not a
/// broken document — `check` must report the placeholder, not fail on it.
///
/// Regression: every other slot answers a `check` hole with a typed zero and
/// builds on, so nothing skipped the build for a root, which cannot produce a
/// symbol from a hole. `check` then reported the analyser's "`root:` names no
/// symbol" — a document error for a document whose only gap was a `--params`
/// value. Worse, the root's own placeholder was recorded nowhere (`RootSpec`
/// re-parses its subtree outside the hole-aware deserializer), so the report
/// did not even name what to pass.
#[test]
fn check_reports_an_unresolved_root_param_rather_than_failing() {
    // Both report `symbol`, by different routes: the bare root is *asserted* to
    // be one (nothing demands a type of a placeholder that is the whole root),
    // while the nested one is *inferred* from the `SymbolName`-typed slot it
    // sits in. The label being the same either way is the point.
    for (name, doc, param) in [
        ("bare", "root: !param SYMBOL\n", "SYMBOL"),
        ("nested", "root: !pick { symbol: !param SYM }\n", "SYM"),
    ] {
        let (_d, path) = scratch_file(
            &format!("root_param_{name}.yml"),
            &format!("{doc}long:\n  enter: !value true\n"),
        );
        let out = Cmd::new("check").arg("strategy").arg(&path).ok();
        assert!(
            out.stdout.contains("1 unset placeholder"),
            "{name}: the placeholder must be counted: {}",
            out.stdout
        );
        assert!(
            out.stdout.contains(&format!("--params {param}=<symbol>")),
            "{name}: and named, with the type it needs: {}",
            out.stdout
        );
        assert!(
            !out.stdout.contains("names no symbol"),
            "{name}: a missing value is not a document error: {}",
            out.stdout
        );
    }
}

/// The same document *run* still errors: `run` substitutes for real, and a
/// required placeholder with no value has nowhere to go.
#[test]
fn a_required_root_param_still_fails_a_run() {
    let (_s, series) = scratch_file("two.csv", &csv(&["BTCUSDT", "ETHUSDT"]));
    let (_d, doc) = scratch_file("root_param.yml", &format!("root: !param SYMBOL\n{NO_ROOT}"));
    let out = Cmd::new("run")
        .arg(&doc)
        .series(&series)
        .args(&["--crypto", "--quiet"])
        .output_dir("default_root_required")
        .fails();
    assert!(
        out.stderr.contains("parameter `SYMBOL` is not set"),
        "{}",
        out.stderr
    );
}

/// A preset carries its `root:` inside the tag's payload, one level down.
#[test]
fn a_preset_defaults_its_root_too() {
    let (_s, series) = scratch_file("one.csv", &csv(&["BTCUSDT"]));
    let (_d, doc) = scratch_file("preset.yml", "!ma_crossover { fast: 3, slow: 10 }\n");
    let out = Cmd::new("run")
        .arg(&doc)
        .series(&series)
        .args(&["--crypto", "--quiet"])
        .output_dir("default_root_preset")
        .ok();
    let fills = fills_of(&out);
    assert!(!fills.is_empty());
    assert!(fills.iter().all(|r| r.contains("BTCUSDT")), "{fills:?}");
}
