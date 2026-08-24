//! `--pooled`: fit **one** parameter set across a panel of instruments instead
//! of picking the best `(params, instrument)` cell.
//!
//! Three properties, and each fails differently if it is wrong:
//!
//! 1. **The axis leaves the ranking.** A pooled sweep emits one row per
//!    parameter set, not one per `(parameter set, member)`, and the pooled axis
//!    is not a CSV column. Without this you have a plain root-axis sweep with
//!    extra steps — and `N×M` hypotheses where you believe you have `N`.
//! 2. **An undefined metric stays undefined.** The pooled mean is over the
//!    members that *reported*, never over zeros substituted for the ones that
//!    could not compute one, and the `_n` column says how many that was. A mean
//!    over 2 of 30 and a mean over 30 of 30 must not render identically.
//! 3. **Folds are on a shared clock.** Instruments list at different dates, so
//!    a fold defined on bar indices spans a different period per member. A
//!    member with no bars in a fold's window contributes nothing to it and does
//!    not shift it.
//!
//! The guard that matters most is #1: a test asserting only "the sweep produced
//! rows" would pass the un-pooled behaviour too, so every assertion here is
//! about the *shape* of what came back, not merely that something did.
//!
//! Also covered: `fugazi run --pooled` — the `run` twin restricted to a single
//! parameter set, sharing the same axis-extraction grammar and the same
//! kept-not-nulled ruin policy — and a regression pin for a real bug this
//! session's own earlier pass introduced: `optimize --pooled` on a
//! `pairs:`/`basket:`/`multi:`/`portfolio:` document used to be silently
//! ignored rather than refused.

mod common;

use common::cli::{Cmd, scratch_file, unique_path};

/// `optimize`'s artefacts are sibling files derived from `--output`'s stem, so
/// the tests need the path itself rather than a scratch *directory* (which is
/// what `run` takes). Returns `(dir, stem_path)`.
fn out_path(name: &str) -> std::path::PathBuf {
    let p = unique_path(name).with_extension("csv");
    let _ = std::fs::remove_file(&p);
    p
}

/// Read a written CSV back as `(header, data rows)`.
fn read_csv(path: &std::path::Path) -> (String, Vec<String>) {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("optimize did not write {}: {e}", path.display()));
    let mut lines = text.lines().map(str::to_string);
    let header = lines.next().unwrap_or_default();
    (header, lines.filter(|l| !l.is_empty()).collect())
}

/// Daily bars for `symbol`, starting `start_day` days after 2024-01-01.
///
/// Flat OHLC (`open == close`) so a market order filling at the next bar's open
/// fills at that bar's close — the same convention the rest of the CLI tests
/// use, and what makes the numbers below hand-checkable.
fn bars(symbol: &str, start_day: u32, closes: &[f64]) -> String {
    let mut out = String::new();
    for (i, c) in closes.iter().enumerate() {
        let day = start_day + i as u32;
        // 2024 is a leap year; stay inside January/February by construction.
        let (month, dom) = if day < 31 {
            (1, day + 1)
        } else {
            (2, day - 30)
        };
        out.push_str(&format!(
            "{symbol},1d,2024-{month:02}-{dom:02}T00:00:00Z,{c},{c},{c},{c},1000\n"
        ));
    }
    out
}

fn frame(members: &[(&str, u32, &[f64])]) -> String {
    let mut out = String::from("symbol,freq,time,open,high,low,close,volume\n");
    for (sym, start, closes) in members {
        out.push_str(&bars(sym, *start, closes));
    }
    out
}

/// A two-bar crossover on whichever series `SYM` names. Deliberately trivial:
/// these tests are about the pooling machinery, not about the strategy.
const DOC: &str = "\
root: !pick { symbol: !param SYM }
long:
  enter: !crosses_above
    lhs: !sma { period: !param FAST }
    rhs: !sma { period: !param SLOW }
  exit: !crosses_below
    lhs: !sma { period: !param FAST }
    rhs: !sma { period: !param SLOW }
sizing: !value 1.0
";

/// A rising series and a falling one, both 24 bars from the same start.
fn two_member_frame() -> String {
    let up: Vec<f64> = (0..24).map(|i| 100.0 + i as f64 * 2.0).collect();
    let down: Vec<f64> = (0..24).map(|i| 150.0 - i as f64 * 2.0).collect();
    frame(&[("UP", 0, &up), ("DOWN", 0, &down)])
}

/// **Property 1.** The pooled axis is reduced over, not ranked on: the sweep
/// emits one row per parameter set and `SYM` is not a column.
///
/// The same grid *without* `--pooled` emits one row per `(params, SYM)` pair
/// and carries a `SYM` column — asserted here too, because it is the behaviour
/// this feature has to be distinguishable from.
#[test]
fn a_pooled_axis_leaves_the_ranking_and_the_columns() {
    let (frame_path, _keep) = scratch_file("pooled_two.csv", &two_member_frame());
    let (doc_path, _keep_doc) = scratch_file("pooled_doc.yml", DOC);

    let pooled_csv = out_path("pooled_axis");
    Cmd::new("optimize")
        .arg(&format!("@{}", doc_path.display()))
        .series(&format!("@{}", frame_path.display()))
        .args(&["--grid", "SYM=[\"UP\",\"DOWN\"],FAST=[2,3]"])
        .args(&["--params", "SLOW=4"])
        .args(&["--pooled", "SYM"])
        .args(&["-m", "returns.total_pct"])
        .args(&["--crypto"])
        .args(&["--output", &pooled_csv.to_string_lossy()])
        .ok();
    let (header, rows) = read_csv(&pooled_csv);

    // FAST has two values, SYM is reduced over -> exactly two rows.
    assert_eq!(
        rows.len(),
        2,
        "pooled sweep must emit one row per parameter set, got:\n{}",
        rows.join("\n")
    );
    assert!(
        !header.split(',').any(|c| c == "SYM"),
        "the pooled axis must not be a CSV column, header was `{header}`"
    );
    assert!(
        header.contains("returns.total_pct_mean")
            && header.contains("returns.total_pct_std")
            && header.contains("returns.total_pct_n"),
        "pooled metrics need mean/std/n columns, header was `{header}`"
    );

    // The control: the same grid, un-pooled, ranks every cell separately.
    let control_csv = out_path("pooled_axis_control");
    Cmd::new("optimize")
        .arg(&format!("@{}", doc_path.display()))
        .series(&format!("@{}", frame_path.display()))
        .args(&["--grid", "SYM=[\"UP\",\"DOWN\"],FAST=[2,3]"])
        .args(&["--params", "SLOW=4"])
        .args(&["-m", "returns.total_pct"])
        .args(&["--crypto"])
        .args(&["--output", &control_csv.to_string_lossy()])
        .ok();
    let (control_header, control_rows) = read_csv(&control_csv);
    assert_eq!(
        control_rows.len(),
        4,
        "without --pooled the grid is N×M — that is the behaviour --pooled exists to change"
    );
    assert!(
        control_header.split(',').any(|c| c == "SYM"),
        "without --pooled, SYM is an ordinary ranked axis and keeps its column"
    );
}

/// **Property 2.** An undefined metric is dropped from the pooled mean rather
/// than counted as zero, and the `_n` column says how many members backed it.
///
/// One row, two metrics, deliberately different support. `WAVY` oscillates, so
/// it crosses and trades; `SILENT` is perfectly flat, so it never crosses and
/// never trades. `trades.total` is therefore defined for **both** (zero is a
/// real trade count) while `trades.win_rate_pct` is defined for **one** — a
/// member with no trades has no win rate at all.
///
/// Asserting both in the same row is what makes this a test of the support
/// mechanism rather than of the fixture: `_n` has to read 2 and 1 respectively,
/// and the win rate has to be `SILENT`-free rather than halved by an invented
/// zero.
#[test]
fn an_undefined_metric_is_dropped_from_the_mean_and_counted() {
    // Zigzag: rises for four bars, falls for four, so SMA(2) crosses SMA(4)
    // repeatedly in both directions.
    let wavy: Vec<f64> = (0..24)
        .map(|i| {
            let phase = i % 8;
            100.0
                + if phase < 4 {
                    phase as f64
                } else {
                    (8 - phase) as f64
                } * 3.0
        })
        .collect();
    let silent = [50.0f64; 24];
    let (frame_path, _keep) = scratch_file(
        "pooled_silent.csv",
        &frame(&[("WAVY", 0, &wavy), ("SILENT", 0, &silent)]),
    );
    let (doc_path, _keep_doc) = scratch_file("pooled_silent_doc.yml", DOC);

    let csv = out_path("pooled_support");
    Cmd::new("optimize")
        .arg(&format!("@{}", doc_path.display()))
        .series(&format!("@{}", frame_path.display()))
        .args(&["--grid", "SYM=[\"WAVY\",\"SILENT\"],FAST=[2]"])
        .args(&["--params", "SLOW=4"])
        .args(&["--pooled", "SYM"])
        .args(&["-m", "trades.total"])
        .args(&["-m", "trades.win_rate_pct"])
        .args(&["--crypto"])
        .args(&["--output", &csv.to_string_lossy()])
        .ok();

    let (header, mut rows) = read_csv(&csv);
    let row = rows.remove(0);
    let cells: Vec<&str> = row.split(',').collect();
    let idx = |name: &str| {
        header
            .split(',')
            .position(|c| c == name)
            .unwrap_or_else(|| panic!("no `{name}` column in `{header}`"))
    };
    let cell = |name: &str| cells[idx(name)];

    // The fixture itself: WAVY must actually trade, or neither assertion means
    // anything.
    let total_mean: f64 = cell("trades.total_mean").parse().expect("numeric");
    assert!(
        total_mean > 0.0,
        "fixture is broken — no member traded, so there is no support gap to measure"
    );

    // Defined for both members: zero trades is still a trade count.
    assert_eq!(
        cell("trades.total_n"),
        "2",
        "trades.total is defined for both members"
    );
    // Defined for one: a member with no trades has no win rate.
    assert_eq!(
        cell("trades.win_rate_pct_n"),
        "1",
        "SILENT never traded, so it must be dropped from the win-rate mean, not zero-filled"
    );
    // The surviving mean is present and rests on one member. Its *value* is
    // whatever that member scored — a whipsawing crossover can legitimately win
    // nothing — so what is asserted is that it was computed at all and that its
    // dispersion is the zero of a one-member sample, not of a consistent panel.
    // `_n` above is the only thing that separates those two readings, which is
    // why it is a column rather than an inference.
    assert!(
        !cell("trades.win_rate_pct_mean").is_empty(),
        "the member that traded reported a win rate, so the pooled cell must not be empty"
    );
    let win_std: f64 = cell("trades.win_rate_pct_std").parse().expect("numeric");
    assert_eq!(
        win_std, 0.0,
        "a mean over a single member has no dispersion — `_n` is what distinguishes \
         it from a genuinely consistent panel"
    );
}

/// **Property 3.** Under `--walkforward`, folds are laid out on the panel's
/// shared clock, so a member that had not listed yet contributes nothing to the
/// early folds instead of shifting them.
///
/// `EARLY` runs the whole span; `LATE` starts 30 bars in. The first fold's
/// in-sample window predates `LATE` entirely, so its member count must be 1
/// while a later fold's is 2. If folds were laid out on per-member bar indices
/// instead, both members would appear in every fold and the counts would be
/// identical — which is exactly the reading `TODO.md` refused to emit.
#[test]
fn ragged_members_contribute_only_to_folds_they_have_bars_in() {
    let early: Vec<f64> = (0..60).map(|i| 100.0 + (i % 7) as f64).collect();
    let late: Vec<f64> = (0..30).map(|i| 100.0 + (i % 5) as f64).collect();
    let (frame_path, _keep) = scratch_file(
        "pooled_ragged.csv",
        &frame(&[("EARLY", 0, &early), ("LATE", 30, &late)]),
    );
    let (doc_path, _keep_doc) = scratch_file("pooled_ragged_doc.yml", DOC);

    let csv = out_path("pooled_ragged");
    Cmd::new("optimize")
        .arg(&format!("@{}", doc_path.display()))
        .series(&format!("@{}", frame_path.display()))
        .args(&["--grid", "SYM=[\"EARLY\",\"LATE\"],FAST=[2,3]"])
        .args(&["--params", "SLOW=4"])
        .args(&["--pooled", "SYM"])
        .args(&["--walkforward", "20,15"])
        .args(&["--best-by", "sharpe"])
        .args(&["-m", "sharpe"])
        .args(&["--crypto"])
        .args(&["--output", &csv.to_string_lossy()])
        .ok();

    let (header, rows) = read_csv(&csv);
    assert!(
        rows.len() >= 2,
        "need at least two folds, got {}",
        rows.len()
    );

    let idx = |name: &str| {
        header
            .split(',')
            .position(|c| c == name)
            .unwrap_or_else(|| panic!("no `{name}` column in `{header}`"))
    };
    let members_of =
        |row: &str, col: &str| -> usize { row.split(',').nth(idx(col)).unwrap().parse().unwrap() };

    let first = members_of(&rows[0], "is_members");
    let last = members_of(rows.last().unwrap(), "oos_members");
    assert_eq!(
        first, 1,
        "LATE has no bars in the first fold's IS window, so it must not be counted in it"
    );
    assert_eq!(
        last, 2,
        "by the final fold both members have bars, so both must be counted"
    );

    // One composite curve per member — the panel does not net into one account.
    let sibling = |suffix: &str| {
        csv.with_file_name(format!(
            "{}.{suffix}",
            csv.file_stem().unwrap().to_string_lossy()
        ))
    };
    assert!(
        sibling("composite_oos_equity.EARLY.csv").exists(),
        "expected a per-member composite for EARLY"
    );
    assert!(
        sibling("composite_oos_equity.LATE.csv").exists(),
        "expected a per-member composite for LATE"
    );
}

/// A pooled axis naming nothing in the grid is bad input, reported with the
/// axes that *are* available rather than a panic or an empty panel.
#[test]
fn a_pooled_axis_that_names_no_grid_axis_is_refused() {
    let (frame_path, _keep) = scratch_file("pooled_bad_axis.csv", &two_member_frame());
    let (doc_path, _keep_doc) = scratch_file("pooled_bad_axis.yml", DOC);

    let out = Cmd::new("optimize")
        .arg(&format!("@{}", doc_path.display()))
        .series(&format!("@{}", frame_path.display()))
        .args(&["--grid", "SYM=[\"UP\",\"DOWN\"],FAST=[2,3]"])
        .args(&["--params", "SLOW=4"])
        .args(&["--pooled", "NOPE"])
        .args(&["--crypto"])
        .args(&["--output", "/dev/null"])
        .fails();
    assert!(
        out.stderr.contains("NOPE") && out.stderr.contains("FAST"),
        "the error should name the missing axis and what is available, got:\n{}",
        out.stderr
    );
}

/// Pooling over a single value is a panel of one, which is a plain sweep with
/// an extra layer of indirection — and, worse, a `_std` of zero that reads like
/// consistency rather than like an absent comparison.
#[test]
fn a_panel_of_one_is_refused() {
    let (frame_path, _keep) = scratch_file("pooled_one.csv", &two_member_frame());
    let (doc_path, _keep_doc) = scratch_file("pooled_one.yml", DOC);

    let out = Cmd::new("optimize")
        .arg(&format!("@{}", doc_path.display()))
        .series(&format!("@{}", frame_path.display()))
        .args(&["--grid", "SYM=[\"UP\"],FAST=[2,3]"])
        .args(&["--params", "SLOW=4"])
        .args(&["--pooled", "SYM"])
        .args(&["--crypto"])
        .args(&["--output", "/dev/null"])
        .fails();
    assert!(
        out.stderr.contains("panel of one"),
        "expected the panel-of-one refusal, got:\n{}",
        out.stderr
    );
}

// ---------------------------------------------------------------------------
// `fugazi run --pooled` — the `run` twin, restricted to one parameter set
// ---------------------------------------------------------------------------
//
// `optimize`'s job is finding the best parameter set; `run`'s is reporting
// what one already-chosen set actually does. `--pooled` on `run` answers that
// across a panel: fit the same document to every member's own data and report
// the panel's `mean ∓ std`, with each member's own artefacts written to its
// own subdirectory so a pooled run is diagnosable one member at a time.

/// A document that shorts unconditionally on the first opportunity and never
/// covers — the same "shortest path to insolvency" recipe `tests/ruin.rs`
/// uses, ported to the spec layer. No `rebalance_on:` (defaults to `!never`),
/// so once the short is opened it is held at a fixed unit count rather than
/// resized bar to bar — which is what lets a large enough adverse move ruin
/// the account outright instead of being continuously de-risked.
const SHORT_FOREVER: &str = "\
root: !pick { symbol: !param SYM }
short:
  enter: !gt { lhs: !value 2, rhs: !value 1 }
  exit: !never
sizing: !value 1.0
";

/// 100 → 100 → 150 → 260 → 320 → 400 → 450 → 500 → 600 — the exact `DOOMED`
/// series `tests/ruin.rs` uses: a >100% adverse move against a fully-invested
/// short, which crosses equity through zero with no leverage knob involved.
const DOOMED: [f64; 9] = [
    100.0, 100.0, 150.0, 260.0, 320.0, 400.0, 450.0, 500.0, 600.0,
];

/// A calm series a short survives easily — the control proving the fixture,
/// not the feature: without a DOOMED-shaped adverse move, the same strategy on
/// the same account does not ruin.
const CALM: [f64; 9] = [100.0, 99.0, 101.0, 100.0, 99.0, 101.0, 100.0, 99.0, 100.0];

#[test]
fn a_pooled_run_writes_one_full_run_per_member_and_a_pooled_reduction() {
    let up: Vec<f64> = (0..24).map(|i| 100.0 + i as f64 * 2.0).collect();
    let down: Vec<f64> = (0..24).map(|i| 150.0 - i as f64 * 2.0).collect();
    let (frame_path, _keep) = scratch_file(
        "run_pooled_two.csv",
        &frame(&[("UP", 0, &up), ("DOWN", 0, &down)]),
    );
    let (doc_path, _keep_doc) = scratch_file("run_pooled_doc.yml", DOC);

    let out = Cmd::new("run")
        .arg(&format!("@{}", doc_path.display()))
        .series(&format!("@{}", frame_path.display()))
        .args(&["--params", "SYM=[\"UP\",\"DOWN\"]"])
        .args(&["--params", "FAST=2"])
        .args(&["--params", "SLOW=4"])
        .args(&["--pooled", "SYM"])
        .args(&["--crypto"])
        .output_dir("run_pooled_two_out")
        .ok();

    // Each member gets its own full `run` output — the same four files a
    // plain `run` would write for that member alone.
    for member in ["UP", "DOWN"] {
        for artefact in ["fills.csv", "trades.csv", "returns.csv", "metrics.yml"] {
            assert!(
                out.wrote(&format!("{member}/{artefact}")),
                "expected {member}/{artefact} to be written"
            );
        }
    }
    // The top-level metrics.yml is the pooled reduction, not a third member's
    // worth of run output — it has to name both members and carry a pooled
    // section, not just be another whole-run Metrics document.
    let pooled_yaml = out.read("metrics.yml");
    assert!(
        pooled_yaml.contains("pooled:"),
        "missing `pooled:` section:\n{pooled_yaml}"
    );
    assert!(
        pooled_yaml.contains("members:"),
        "missing `members:` section:\n{pooled_yaml}"
    );
    assert!(
        pooled_yaml.contains("UP:"),
        "pooled doc must name member UP:\n{pooled_yaml}"
    );
    assert!(
        pooled_yaml.contains("DOWN:"),
        "pooled doc must name member DOWN:\n{pooled_yaml}"
    );

    // The console names the panel and both members.
    assert!(out.stdout.contains("pooled"), "{}", out.stdout);
    assert!(out.stdout.contains("member UP"), "{}", out.stdout);
    assert!(out.stdout.contains("member DOWN"), "{}", out.stdout);
}

/// A ruined member's pre-ruin numbers are folded into the pooled mean rather
/// than dropped — the same "kept, not nulled" rule a single ruined run's own
/// `metrics.yml` follows — and the console names which member ruined.
///
/// This is a control-and-treatment pair rather than one member: DOOMED proves
/// the strategy really can ruin under this document, CALM proves it does not
/// ruin *unconditionally* — without the control, a bug that ruined every
/// member (or none) would pass just as easily.
#[test]
fn a_ruined_members_pre_ruin_numbers_are_kept_and_named() {
    let (frame_path, _keep) = scratch_file(
        "run_pooled_ruin.csv",
        &frame(&[("DOOMED", 0, &DOOMED), ("CALM", 0, &CALM)]),
    );
    let (doc_path, _keep_doc) = scratch_file("run_pooled_ruin_doc.yml", SHORT_FOREVER);

    let out = Cmd::new("run")
        .arg(&format!("@{}", doc_path.display()))
        .series(&format!("@{}", frame_path.display()))
        .args(&["--params", "SYM=[\"DOOMED\",\"CALM\"]"])
        .args(&["--pooled", "SYM"])
        .args(&["--crypto"])
        .output_dir("run_pooled_ruin_out")
        .ok();

    // `ruin_bar` is `#[serde(skip_serializing_if = "Option::is_none")]` — a
    // solvent run has no `ruin_bar:` line at all, not one reading `null`.
    let doomed_yaml = out.read("DOOMED/metrics.yml");
    assert!(
        doomed_yaml.contains("ruin_bar:"),
        "DOOMED must have ruined — a >100% adverse move against a fully-invested, \
         never-covered short is exactly the recipe tests/ruin.rs pins as ruin:\n{doomed_yaml}"
    );
    let calm_yaml = out.read("CALM/metrics.yml");
    assert!(
        !calm_yaml.contains("ruin_bar:"),
        "CALM is the control — it must survive the same strategy on a flat series:\n{calm_yaml}"
    );

    // The pooled mean is still computed — DOOMED's pre-ruin numbers are kept,
    // not nulled, exactly as a single ruined run's own cells stay.
    let pooled_yaml = out.read("metrics.yml");
    assert!(
        pooled_yaml.contains("pooled:"),
        "pooling must still produce a reduction despite one ruined member:\n{pooled_yaml}"
    );

    // The console names the ruined member rather than leaving it implicit.
    assert!(
        out.stdout.contains("ruined") && out.stdout.contains("DOOMED"),
        "console must name the ruined member:\n{}",
        out.stdout
    );
}

/// The four `--pooled` refusals `run` shares with `optimize`'s axis-extraction
/// discipline: a missing axis, a single-value axis, another axis-shaped
/// `--params` entry, and (the case `optimize` has no equivalent for)
/// composing with state that has no per-member meaning yet.
#[test]
fn run_pooled_refusals() {
    let (frame_path, _keep) = scratch_file("run_pooled_refusals.csv", &two_member_frame());
    let (doc_path, _keep_doc) = scratch_file("run_pooled_refusals.yml", DOC);
    let base = || {
        Cmd::new("run")
            .arg(&format!("@{}", doc_path.display()))
            .series(&format!("@{}", frame_path.display()))
            .args(&["--crypto"])
    };

    let missing_axis = base()
        .args(&["--params", "FAST=2"])
        .args(&["--params", "SLOW=4"])
        .args(&["--pooled", "SYM"])
        .args(&["--output-dir", "/dev/null"])
        .fails();
    assert!(
        missing_axis.stderr.contains("names no --params entry"),
        "{}",
        missing_axis.stderr
    );

    let single_value = base()
        .args(&["--params", "SYM=[\"UP\"]"])
        .args(&["--params", "FAST=2"])
        .args(&["--params", "SLOW=4"])
        .args(&["--pooled", "SYM"])
        .args(&["--output-dir", "/dev/null"])
        .fails();
    assert!(
        single_value.stderr.contains("only 1 value"),
        "{}",
        single_value.stderr
    );

    let other_axis = base()
        .args(&["--params", "SYM=[\"UP\",\"DOWN\"]"])
        .args(&["--params", "FAST=[2,3]"])
        .args(&["--params", "SLOW=4"])
        .args(&["--pooled", "SYM"])
        .args(&["--output-dir", "/dev/null"])
        .fails();
    assert!(
        other_axis.stderr.contains("only accepts scalar values"),
        "{}",
        other_axis.stderr
    );

    let with_flatten = base()
        .args(&["--params", "SYM=[\"UP\",\"DOWN\"]"])
        .args(&["--params", "FAST=2"])
        .args(&["--params", "SLOW=4"])
        .args(&["--pooled", "SYM"])
        .args(&["--flatten"])
        .args(&["--output-dir", "/dev/null"])
        .fails();
    assert!(
        with_flatten
            .stderr
            .contains("doesn't compose with --resume/--save-state/--flatten"),
        "{}",
        with_flatten.stderr
    );
}

// ---------------------------------------------------------------------------
// Regression: `optimize --pooled` on a non-single shape must refuse, not
// silently sweep the axis as an ordinary ranked one.
// ---------------------------------------------------------------------------

/// A minimal, parseable `pairs:` document — the refusal fires on the strategy
/// **kind** (from the `pairs:` source prefix) before the body is even typed,
/// but a document that fails to parse for unrelated reasons would make that
/// hard to tell apart from this check working, so this one is valid.
const PAIRS_DOC: &str = "\
left: AAA
right: BBB
enter: !crosses_above
  lhs: !close { source: !pick { symbol: AAA } }
  rhs: !close { source: !pick { symbol: BBB } }
exit: !never
sizing: !value 1.0
";

#[test]
fn optimize_pooled_on_a_pairs_document_is_refused_not_silently_ignored() {
    let (frame_path, _keep) = scratch_file("pooled_pairs.csv", &two_member_frame());
    let (doc_path, _keep_doc) = scratch_file("pooled_pairs.yml", PAIRS_DOC);

    let out = Cmd::new("optimize")
        .arg(&format!("pairs:@{}", doc_path.display()))
        .series(&format!("@{}", frame_path.display()))
        .args(&["--grid", "FAST=[2,3]"])
        .args(&["--pooled", "FAST"])
        .args(&["--crypto"])
        .args(&["--output", "/dev/null"])
        .fails();
    assert!(
        out.stderr.contains("only wired for single-asset") && out.stderr.contains("pairs:"),
        "expected an explicit refusal naming the shape, got:\n{}",
        out.stderr
    );
}
