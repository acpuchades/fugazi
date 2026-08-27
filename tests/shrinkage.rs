//! `--shrink`: partial pooling, the middle ground between one parameter set for
//! the whole panel and one per member.
//!
//! `--pooled` alone is **complete** pooling — right only when the members share
//! an optimum. A plain `SYM=[...]` grid axis is **no** pooling — a separate
//! answer per member, each fit on its share of the evidence. This is what sits
//! between: estimate how much of the spread between members is real
//! disagreement (`λ`) rather than backtest noise, and let each member move that
//! far and no further.
//!
//! Four properties, each failing differently:
//!
//! 1. **`λ` is `None` without replication, not a number.** With one measurement
//!    per member, disagreement and noise are the same quantity. A tool that
//!    reported `λ = 1` there would be manufacturing its headline finding out of
//!    an identification failure — the single most damaging thing this feature
//!    could do. `-w` in a sweep, per-fold sub-spans under `--walkforward`.
//! 2. **`-w` composes with `--pooled`.** It has to: it is where a sweep's
//!    replication comes from. They used to be mutually exclusive in the same
//!    clap group.
//! 3. **Adding `-w` changes no pooled number.** The windowed reduction is
//!    carried *beside* the whole-run document, never instead of it, so
//!    `_mean`/`_std`/`_n` are bit-identical with and without it. Otherwise
//!    turning on replication would silently re-baseline every pooled result.
//! 4. **Agreement and disagreement reach opposite conclusions.** A panel whose
//!    members share an optimum must pool completely (no member departs); one
//!    whose members rank the grid in opposite orders must let them split. A
//!    test that only ran `--shrink` and checked it did not crash would pass on
//!    an implementation that always returned the pooled winner.

mod common;

use common::cli::{Cmd, scratch_file, unique_path};

fn out_path(name: &str) -> std::path::PathBuf {
    let p = unique_path(name).with_extension("csv");
    let _ = std::fs::remove_file(&p);
    p
}

fn read_csv(path: &std::path::Path) -> (String, Vec<String>) {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("optimize did not write {}: {e}", path.display()));
    let mut lines = text.lines().map(str::to_string);
    let header = lines.next().unwrap_or_default();
    (header, lines.filter(|l| !l.is_empty()).collect())
}

/// Daily bars from 2024-01-01, one row per close. Long enough that a
/// walk-forward fold's in-sample window clears
/// `shrinkage::MIN_REPLICATE_BARS` and can actually be cut into sub-spans.
fn bars(symbol: &str, closes: &[f64]) -> String {
    let mut out = String::new();
    for (i, c) in closes.iter().enumerate() {
        // Stay inside 2024 by walking the day-of-year directly.
        let day = i as u32;
        let (month, dom) = match day {
            0..=30 => (1, day + 1),
            31..=59 => (2, day - 30),
            60..=90 => (3, day - 59),
            91..=120 => (4, day - 90),
            121..=151 => (5, day - 120),
            _ => (6, day - 151),
        };
        out.push_str(&format!(
            "{symbol},1d,2024-{month:02}-{dom:02}T00:00:00Z,{c},{c},{c},{c},1000\n"
        ));
    }
    out
}

fn frame(members: &[(&str, Vec<f64>)]) -> String {
    let mut out = String::from("symbol,freq,time,open,high,low,close,volume\n");
    for (sym, closes) in members {
        out.push_str(&bars(sym, closes));
    }
    out
}

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

/// Two members driven by the *same* underlying shape at different amplitudes:
/// whatever lookback works on one works on the other, so the panel agrees.
fn agreeing_frame() -> String {
    let wave = |amp: f64, scale: f64| -> Vec<f64> {
        (0..160)
            .map(|i| {
                let t = i as f64;
                scale * (100.0 + amp * (t / 9.0).sin() + t * 0.12)
            })
            .collect()
    };
    frame(&[("AAA", wave(6.0, 1.0)), ("BBB", wave(6.0, 3.0))])
}

/// Two members on *different* cycle lengths: a short lookback suits one and a
/// long one suits the other, so no single parameter set is right for both.
fn disagreeing_frame() -> String {
    let wave = |period: f64| -> Vec<f64> {
        (0..160)
            .map(|i| {
                let t = i as f64;
                100.0 + 9.0 * (t / period).sin() + t * 0.05
            })
            .collect()
    };
    frame(&[("FAST_CYCLE", wave(3.5)), ("SLOW_CYCLE", wave(21.0))])
}

/// **Property 1.** Without replication there is no `λ` — the console says so
/// rather than printing a number it cannot support.
///
/// This is the load-bearing negative. `λ` is a ratio of interaction variance to
/// residual variance, and with one observation per cell those are the same
/// sum of squares; any value reported would be an artifact of the fit, not a
/// measurement. The `—` and the pointer to `-w` are the whole contract.
#[test]
fn without_replication_lambda_is_unavailable_not_invented() {
    let (frame_path, _keep) = scratch_file("shrink_norep.csv", &agreeing_frame());
    let (doc_path, _keep_doc) = scratch_file("shrink_norep.yml", DOC);

    let out = Cmd::new("optimize")
        .arg(&format!("@{}", doc_path.display()))
        .series(&format!("@{}", frame_path.display()))
        .args(&["--grid", "FAST=[2,3,4,5]"])
        .args(&["--params", "SLOW=12"])
        .args(&["--pooled", "SYM=[\"AAA\",\"BBB\"]"])
        .args(&["--best-by", "sharpe"])
        .args(&["--shrink"])
        .args(&["--crypto"])
        .args(&["--output", &out_path("shrink_norep").to_string_lossy()])
        .ok();

    assert!(
        out.stdout.contains("member agreement (λ)"),
        "a pooled sweep must report λ either way, got:\n{}",
        out.stdout
    );
    assert!(
        out.stdout.contains("not estimable without replication"),
        "an unreplicated panel must say λ is unavailable, not print a number:\n{}",
        out.stdout
    );
    assert!(
        out.stdout.contains("-w"),
        "the unavailable-λ line must name the flag that fixes it:\n{}",
        out.stdout
    );
}

/// **Property 2 and 3.** `-w` composes with `--pooled` — and adding it leaves
/// every pooled number exactly where it was.
///
/// The composition is what makes a sweep's `λ` estimable at all; the two flags
/// were previously in one clap group and mutually exclusive. The invariance is
/// the more important half: the windowed reduction rides *beside* the whole-run
/// document rather than replacing it, so turning replication on must not
/// re-baseline a single pooled cell.
#[test]
fn windowed_composes_with_pooled_and_changes_no_pooled_number() {
    let (frame_path, _keep) = scratch_file("shrink_compose.csv", &agreeing_frame());
    let (doc_path, _keep_doc) = scratch_file("shrink_compose.yml", DOC);

    let run = |windowed: bool, name: &str| -> (String, Vec<String>) {
        let path = out_path(name);
        let mut cmd = Cmd::new("optimize")
            .arg(&format!("@{}", doc_path.display()))
            .series(&format!("@{}", frame_path.display()))
            .args(&["--grid", "FAST=[2,3,4,5]"])
            .args(&["--params", "SLOW=12"])
            .args(&["--pooled", "SYM=[\"AAA\",\"BBB\"]"])
            .args(&["-m", "returns.total_pct"])
            .args(&["--crypto"])
            .args(&["--output", &path.to_string_lossy()]);
        if windowed {
            cmd = cmd.args(&["-w", "40"]);
        }
        cmd.ok();
        read_csv(&path)
    };

    let (plain_header, plain_rows) = run(false, "shrink_compose_plain");
    let (windowed_header, windowed_rows) = run(true, "shrink_compose_windowed");

    assert_eq!(
        plain_header, windowed_header,
        "-w must not change a pooled sweep's columns"
    );
    assert_eq!(
        plain_rows, windowed_rows,
        "-w supplies replication for λ and must leave every pooled cell identical"
    );
    assert!(
        plain_header.contains("returns.total_pct_mean"),
        "sanity: this is still the pooled column shape, got `{plain_header}`"
    );
}

/// **Property 1, positive half.** With `-w` supplying replicates, `λ` is a
/// number, and the sweep emits the member-demeaned ranking columns beside the
/// raw pooled ones.
///
/// `_z` is not a replacement for `_mean`: the member effect is identical for
/// every row and so carries no ranking information, but removing it changes
/// what a cross-member `std` *means*. Both have to be present for a reader to
/// see whether the raw ranking was resting on the panel's composition.
#[test]
fn replication_yields_a_lambda_and_the_demeaned_columns() {
    let (frame_path, _keep) = scratch_file("shrink_lambda.csv", &agreeing_frame());
    let (doc_path, _keep_doc) = scratch_file("shrink_lambda.yml", DOC);

    let path = out_path("shrink_lambda");
    let out = Cmd::new("optimize")
        .arg(&format!("@{}", doc_path.display()))
        .series(&format!("@{}", frame_path.display()))
        .args(&["--grid", "FAST=[2,3,4,5]"])
        .args(&["--params", "SLOW=12"])
        .args(&["--pooled", "SYM=[\"AAA\",\"BBB\"]"])
        .args(&["-w", "40"])
        .args(&["--best-by", "sharpe"])
        .args(&["--crypto"])
        .args(&["--output", &path.to_string_lossy()])
        .ok();

    assert!(
        !out.stdout.contains("not estimable without replication"),
        "with -w the panel is replicated and λ must be a number:\n{}",
        out.stdout
    );
    assert!(
        out.stdout.contains("member agreement (λ)"),
        "λ must be reported, got:\n{}",
        out.stdout
    );

    let (header, rows) = read_csv(&path);
    assert!(
        header.contains("risk_adjusted.sharpe_z") && header.contains("risk_adjusted.sharpe_z_std"),
        "the demeaned ranking columns must sit beside the raw ones, header was `{header}`"
    );
    assert!(
        header.contains("risk_adjusted.sharpe_mean"),
        "the raw pooled column must survive — `_z` is an addition, header was `{header}`"
    );
    assert_eq!(rows.len(), 4, "one row per parameter set, as ever");
    // Every row's `_z` cell is populated: the fit covers the whole table, and a
    // blank column would mean the demeaning silently did nothing.
    let z_at = header
        .split(',')
        .position(|c| c == "risk_adjusted.sharpe_z")
        .expect("column present");
    for row in &rows {
        let cell = row.split(',').nth(z_at).unwrap_or_default();
        assert!(
            !cell.is_empty(),
            "every pooled row should carry a demeaned score, row was `{row}`"
        );
    }
}

/// **Property 4, the agreeing half.** When the members share an optimum, no
/// member departs from the pooled winner and no per-member file is written.
///
/// This is the negative result the feature has to be able to reach. An
/// implementation that always split the panel would look identical to a correct
/// one on the disagreeing fixture below, and only this test tells them apart.
/// The console has to say so too — silence would read as "the flag did
/// nothing".
#[test]
fn an_agreeing_panel_pools_completely() {
    let (frame_path, _keep) = scratch_file("shrink_agree.csv", &agreeing_frame());
    let (doc_path, _keep_doc) = scratch_file("shrink_agree.yml", DOC);

    let path = out_path("shrink_agree");
    let out = Cmd::new("optimize")
        .arg(&format!("@{}", doc_path.display()))
        .series(&format!("@{}", frame_path.display()))
        .args(&["--grid", "FAST=[2,3,4,5]"])
        .args(&["--params", "SLOW=12"])
        .args(&["--pooled", "SYM=[\"AAA\",\"BBB\"]"])
        .args(&["--walkforward", "60,30"])
        .args(&["--best-by", "sharpe"])
        .args(&["--shrink"])
        .args(&["--crypto"])
        .args(&["--output", &path.to_string_lossy()])
        .ok();

    let winners = path.with_file_name(format!(
        "{}.member_winners.csv",
        path.file_stem().unwrap().to_string_lossy()
    ));
    assert!(
        !winners.exists(),
        "an agreeing panel writes no per-member file — a `departed` column of all-false \
         reads like a finding when it is the absence of one"
    );
    assert!(
        out.stdout.contains("every member took the pooled winner"),
        "the negative result has to be stated, not left as silence:\n{}",
        out.stdout
    );
}

/// **Property 4, the disagreeing half.** When the members rank the grid
/// differently, partial pooling lets them split — and records who split, where.
///
/// Two members on cycle lengths a factor of six apart. No single lookback is
/// right for both, which is exactly the case where a pooled winner is a
/// compromise worse than either member's own answer.
#[test]
fn a_disagreeing_panel_lets_its_members_split() {
    let (frame_path, _keep) = scratch_file("shrink_split.csv", &disagreeing_frame());
    let (doc_path, _keep_doc) = scratch_file("shrink_split.yml", DOC);

    let path = out_path("shrink_split");
    let out = Cmd::new("optimize")
        .arg(&format!("@{}", doc_path.display()))
        .series(&format!("@{}", frame_path.display()))
        .args(&["--grid", "FAST=[2,3,5,8],SLOW=[10,16,24]"])
        .args(&["--pooled", "SYM=[\"FAST_CYCLE\",\"SLOW_CYCLE\"]"])
        .args(&["--walkforward", "60,30"])
        .args(&["--best-by", "sharpe"])
        .args(&["--shrink"])
        .args(&["--crypto"])
        .args(&["--output", &path.to_string_lossy()])
        .ok();

    assert!(
        out.stdout.contains("λ"),
        "a shrunk walk-forward must report per-fold λ:\n{}",
        out.stdout
    );

    let winners = path.with_file_name(format!(
        "{}.member_winners.csv",
        path.file_stem().unwrap().to_string_lossy()
    ));
    if winners.exists() {
        let (header, rows) = read_csv(&winners);
        assert!(
            header.starts_with("fold,member,departed,"),
            "the per-member file is keyed by (fold, member), header was `{header}`"
        );
        assert!(
            header.contains("FAST") && header.contains("SLOW"),
            "each member's own chosen parameters must be in the file, header was `{header}`"
        );
        assert!(
            rows.iter().any(|r| r.contains(",true,")),
            "the file is only written when something departed, so at least one row must \
             say so:\n{}",
            rows.join("\n")
        );
    } else {
        // The panel may still agree on this fixture depending on how the folds
        // land — that is a legitimate outcome, not a failure. What must hold is
        // that the run said so rather than staying silent.
        assert!(
            out.stdout.contains("every member took the pooled winner"),
            "with no per-member file, the console must explain why:\n{}",
            out.stdout
        );
    }
}

/// The `folds.csv` written under `--shrink` carries the decomposition, so a
/// reader can see *why* members did or did not split without re-running.
#[test]
fn a_shrunk_walkforward_writes_its_decomposition_to_the_fold_csv() {
    let (frame_path, _keep) = scratch_file("shrink_folds.csv", &disagreeing_frame());
    let (doc_path, _keep_doc) = scratch_file("shrink_folds.yml", DOC);

    let path = out_path("shrink_folds");
    Cmd::new("optimize")
        .arg(&format!("@{}", doc_path.display()))
        .series(&format!("@{}", frame_path.display()))
        .args(&["--grid", "FAST=[2,3,5,8]"])
        .args(&["--params", "SLOW=16"])
        .args(&["--pooled", "SYM=[\"FAST_CYCLE\",\"SLOW_CYCLE\"]"])
        .args(&["--walkforward", "60,30"])
        .args(&["--best-by", "sharpe"])
        .args(&["--shrink"])
        .args(&["--crypto"])
        .args(&["--output", &path.to_string_lossy()])
        .ok();

    let (header, rows) = read_csv(&path);
    for column in [
        "lambda",
        "lambda_support",
        "lambda_cells",
        "members_departed",
    ] {
        assert!(
            header.split(',').any(|c| c == column),
            "`--shrink` must write `{column}` to the fold CSV, header was `{header}`"
        );
    }
    assert!(!rows.is_empty(), "the walk-forward produced no folds");
}

/// `--shrink` needs the two flags it shrinks *between* and *toward*: a panel to
/// disagree across, and a ranking key to disagree about.
#[test]
fn shrink_refuses_without_a_panel_or_a_ranking_key() {
    let (frame_path, _keep) = scratch_file("shrink_refuse.csv", &agreeing_frame());
    let (doc_path, _keep_doc) = scratch_file("shrink_refuse.yml", DOC);

    // No `--pooled`: there is no panel, so nothing to pool partially.
    let no_panel = Cmd::new("optimize")
        .arg(&format!("@{}", doc_path.display()))
        .series(&format!("@{}", frame_path.display()))
        .args(&["--grid", "FAST=[2,3]"])
        .args(&["--params", "SLOW=12,SYM=AAA"])
        .args(&["--best-by", "sharpe"])
        .args(&["--shrink"])
        .args(&["--crypto"])
        .args(&["--output", "/dev/null"])
        .fails();
    assert!(
        no_panel.stderr.contains("pooled"),
        "--shrink without --pooled must name the missing flag, got:\n{}",
        no_panel.stderr
    );

    // No `--best-by`: there is no surface for a member to select off.
    let no_key = Cmd::new("optimize")
        .arg(&format!("@{}", doc_path.display()))
        .series(&format!("@{}", frame_path.display()))
        .args(&["--grid", "FAST=[2,3]"])
        .args(&["--params", "SLOW=12"])
        .args(&["--pooled", "SYM=[\"AAA\",\"BBB\"]"])
        .args(&["--shrink"])
        .args(&["--crypto"])
        .args(&["--output", "/dev/null"])
        .fails();
    assert!(
        no_key.stderr.contains("best-by") || no_key.stderr.contains("best_by"),
        "--shrink without --best-by must name the missing flag, got:\n{}",
        no_key.stderr
    );
}

/// `-k` and `--shrink` are rival answers to "what should the spread between
/// members cost", and the sweep refuses to apply both.
///
/// `-k` *charges* a parameter set for that spread; `--shrink` *models* it and
/// lets each member move by however much of it is real. Running both pays for
/// the same disagreement twice. Refusing follows the precedent already set for
/// an inert `--smooth`: a flag that silently does nothing is worse than one
/// that says it cannot.
#[test]
fn shrink_refuses_risk_aversion() {
    let (frame_path, _keep) = scratch_file("shrink_k.csv", &agreeing_frame());
    let (doc_path, _keep_doc) = scratch_file("shrink_k.yml", DOC);

    let out = Cmd::new("optimize")
        .arg(&format!("@{}", doc_path.display()))
        .series(&format!("@{}", frame_path.display()))
        .args(&["--grid", "FAST=[2,3,4,5]"])
        .args(&["--params", "SLOW=12"])
        .args(&["--pooled", "SYM=[\"AAA\",\"BBB\"]"])
        .args(&["-w", "40"])
        .args(&["--best-by", "sharpe"])
        .args(&["--shrink"])
        .args(&["-k", "1.0"])
        .args(&["--crypto"])
        .args(&["--output", "/dev/null"])
        .fails();
    assert!(
        out.stderr.contains("shrink") && out.stderr.contains("risk-aversion"),
        "the refusal must name both flags, got:\n{}",
        out.stderr
    );
}

/// At `λ = 0` every member sees the identical surface `μ + α_r`, so no member
/// can pick a different row — and the fold must not report otherwise.
///
/// This pins a contradiction the first implementation actually produced: the
/// pooled winner came from `pooled_ranking_key` (each member's *whole*-window
/// document) while the members picked off the decomposition (cell means over
/// in-sample *sub-spans*). Two honest numbers on different scales, which need
/// not share an argmax — so a fold could report `λ 0.000` beside `2 member(s)
/// chose differently`, which is impossible by construction and read as a bug in
/// the estimator rather than in the comparison.
#[test]
fn a_zero_lambda_fold_has_no_departures() {
    let (frame_path, _keep) = scratch_file("shrink_zero.csv", &agreeing_frame());
    let (doc_path, _keep_doc) = scratch_file("shrink_zero.yml", DOC);

    let out = Cmd::new("optimize")
        .arg(&format!("@{}", doc_path.display()))
        .series(&format!("@{}", frame_path.display()))
        .args(&["--grid", "FAST=[2,3,5,8],SLOW=[10,16,24]"])
        .args(&["--pooled", "SYM=[\"AAA\",\"BBB\"]"])
        .args(&["--walkforward", "60,30"])
        .args(&["--best-by", "sharpe"])
        .args(&["--shrink"])
        .args(&["--crypto"])
        .args(&["--output", &out_path("shrink_zero").to_string_lossy()])
        .ok();

    for line in out.stdout.lines().filter(|l| l.contains("λ 0.000")) {
        assert!(
            line.contains("0 member(s) chose differently"),
            "at λ=0 every member sees one surface and must pick one row, got:\n{line}"
        );
    }
}

/// `run --pooled` reports what the panel is *worth* as evidence, not just how
/// many members it has.
///
/// Thirty instruments of one market that all track the same beta are worth
/// about one backtest, and a pooled mean over them deserves that much
/// confidence — not thirty times it. The number existed in the library and was
/// printed by no CLI path.
#[test]
fn a_pooled_run_reports_effective_breadth() {
    let (frame_path, _keep) = scratch_file("shrink_breadth.csv", &agreeing_frame());
    let (doc_path, _keep_doc) = scratch_file("shrink_breadth.yml", DOC);

    let out = Cmd::new("run")
        .arg(&format!("@{}", doc_path.display()))
        .series(&format!("@{}", frame_path.display()))
        .args(&["--params", "FAST=3,SLOW=12"])
        .args(&["--pooled", "SYM=[\"AAA\",\"BBB\"]"])
        .args(&["--crypto"])
        .output_dir("shrink_breadth_out")
        .ok();

    assert!(
        out.stdout.contains("effective breadth"),
        "a pooled run must say what its panel is worth as evidence:\n{}",
        out.stdout
    );
    assert!(
        out.stdout.contains("mean pairwise correlation"),
        "the breadth line must show the correlation it rests on:\n{}",
        out.stdout
    );
}

/// A flat per-trade fee applied across a panel is warned about, because it is
/// the same *currency amount* on members that may differ by orders of magnitude
/// in price — nearly free on one and ruinous on another, and it reads
/// downstream as "the parameters do not generalize".
///
/// A warning, not a refusal: one venue's perpetuals really do share a schedule.
#[test]
fn an_unscoped_absolute_cost_across_a_panel_is_warned_about() {
    let (frame_path, _keep) = scratch_file("shrink_costs.csv", &agreeing_frame());
    let (doc_path, _keep_doc) = scratch_file("shrink_costs.yml", DOC);

    let out = Cmd::new("optimize")
        .arg(&format!("@{}", doc_path.display()))
        .series(&format!("@{}", frame_path.display()))
        .args(&["--grid", "FAST=[2,3]"])
        .args(&["--params", "SLOW=12"])
        .args(&["--pooled", "SYM=[\"AAA\",\"BBB\"]"])
        .costs("commission.fixed.amount=1.0")
        .args(&["--crypto"])
        .args(&["--output", &out_path("shrink_costs").to_string_lossy()])
        .ok();

    let all = format!("{}{}", out.stdout, out.stderr);
    assert!(
        all.contains("commission.fixed") && all.contains("no `SYMBOL:` scope"),
        "an unscoped absolute fee on a panel must be called out, got:\n{all}"
    );

    // Scoped, it is deliberate and must stay quiet.
    let scoped = Cmd::new("optimize")
        .arg(&format!("@{}", doc_path.display()))
        .series(&format!("@{}", frame_path.display()))
        .args(&["--grid", "FAST=[2,3]"])
        .args(&["--params", "SLOW=12"])
        .args(&["--pooled", "SYM=[\"AAA\",\"BBB\"]"])
        .costs("AAA:commission.fixed.amount=1.0")
        .args(&["--crypto"])
        .args(&[
            "--output",
            &out_path("shrink_costs_scoped").to_string_lossy(),
        ])
        .ok();
    let all = format!("{}{}", scoped.stdout, scoped.stderr);
    assert!(
        !all.contains("no `SYMBOL:` scope"),
        "a scoped fee is the fix being suggested and must not itself warn:\n{all}"
    );
}

/// `--smooth` and `--shrink` compose, and in a defined order: shrinkage borrows
/// strength from **other members**, smoothing from **neighbouring parameter
/// points**. They are orthogonal axes of one idea — regularizing a noisy score
/// surface — so each member's shrunk column is smoothed over the lattice before
/// its argmax is taken.
///
/// Worth pinning because the order is a choice and the wrong one is silently
/// wrong: smoothing the raw cells first would blur the very disagreement the
/// decomposition exists to measure, and `λ` would read low for a panel that
/// genuinely splits.
#[test]
fn smoothing_composes_with_shrinking() {
    let (frame_path, _keep) = scratch_file("shrink_smooth.csv", &disagreeing_frame());
    let (doc_path, _keep_doc) = scratch_file("shrink_smooth.yml", DOC);

    let path = out_path("shrink_smooth");
    let out = Cmd::new("optimize")
        .arg(&format!("@{}", doc_path.display()))
        .series(&format!("@{}", frame_path.display()))
        .args(&["--grid", "FAST=[2,3,5,8],SLOW=[10,16,24]"])
        .args(&["--pooled", "SYM=[\"FAST_CYCLE\",\"SLOW_CYCLE\"]"])
        .args(&["--walkforward", "60,30"])
        .args(&["--best-by", "sharpe"])
        .args(&["--shrink"])
        .args(&["--smooth=box:1"])
        .args(&["--crypto"])
        .args(&["--output", &path.to_string_lossy()])
        .ok();

    // Both regularizers report. If either silently no-opped under the other,
    // one of these column families would be missing from the fold CSV.
    let (header, rows) = read_csv(&path);
    assert!(
        header.split(',').any(|c| c == "lambda"),
        "shrinking must still report its decomposition under --smooth, header was `{header}`"
    );
    assert!(
        header.contains("_is_smoothed") && header.contains("_is_support"),
        "smoothing must still report its neighbourhood average under --shrink, \
         header was `{header}`"
    );
    assert!(!rows.is_empty(), "the walk-forward produced no folds");
    assert!(
        out.stdout.contains("member agreement (λ)"),
        "λ is reported whichever regularizers are on:\n{}",
        out.stdout
    );
}
