//! End-to-end tests of `--from` / `--until` / `--strict-from`.
//!
//! Three claims are load-bearing enough to pin here, because each is invisible
//! in the output and would be found only by someone re-deriving it by hand:
//!
//! 1. **The interval is half-open.** `--until X` and `--from X` partition a
//!    series exactly — no bar counted twice, none dropped between them. This is
//!    what makes a development / holdout split trustworthy at all.
//! 2. **`--from` bounds evaluation, not loading.** Bars before it warm the
//!    chains without trading, so a sliced run measures settled indicators. The
//!    testable form of that claim is *invariance*: the same `--from` over a
//!    series with more leading history must produce the same numbers, because
//!    the extra history beyond `stable_bars` cannot matter.
//! 3. **`metrics.yml` says what it measured.** `period_start` / `period_end` /
//!    `warmup_bars` describe the evaluated range, including when a short
//!    history forced evaluation to start later than asked.

mod common;

use common::cli::{Cmd, Outcome, at, scratch_file};

/// A daily OHLCV series of `n` bars from 2024-01-01, with a price path that
/// crosses often enough for an SMA crossover to trade on it.
///
/// The wave is deterministic and has no flat stretches, so a slice taken
/// anywhere still produces signals — a test that silently measured zero trades
/// would pass every assertion below for the wrong reason.
fn wavy_series(n: usize) -> String {
    let mut csv = String::from("symbol,time,open,high,low,close,volume\n");
    for i in 0..n {
        let day = time::Date::from_calendar_date(2024, time::Month::January, 1)
            .expect("valid date")
            .saturating_add(time::Duration::days(i as i64));
        // Two interfering periods, so fast and slow SMAs keep crossing.
        let px = 100.0 + 8.0 * ((i as f64) / 5.0).sin() + 3.0 * ((i as f64) / 11.0).cos();
        csv.push_str(&format!(
            "BTC,{day},{px:.4},{:.4},{:.4},{px:.4},1000\n",
            px + 1.0,
            px - 1.0
        ));
    }
    csv
}

/// An **EMA** crossover, deliberately not the SMA example.
///
/// The warm-up claims below are only testable against an indicator with
/// infinite impulse response: an SMA forgets everything older than its window,
/// so a cold start and a warmed start converge after `period` bars and no
/// assertion here could tell them apart. An EMA never fully forgets, so how
/// many bars were read back before the first evaluated one is visible in the
/// result.
const EMA_CROSS: &str = "\
root: BTC
long:
  enter: !crosses_above { lhs: !ema { source: close, period: 5 }, rhs: !ema { source: close, period: 20 } }
short:
  enter: !crosses_below { lhs: !ema { source: close, period: 5 }, rhs: !ema { source: close, period: 20 } }
";

/// `fugazi run` over `series` with the SMA-crossover example, plus `extra`.
fn run_over(series: &str, extra: &[&str], out: &str) -> Outcome {
    run_spec(&at("examples/strategy.yml"), series, extra, out)
}

/// `fugazi run` of an arbitrary strategy spec over `series`.
fn run_spec(spec: &str, series: &str, extra: &[&str], out: &str) -> Outcome {
    Cmd::new("run")
        .arg(spec)
        .series(series)
        .args(&["--crypto", "-f", "1d", "--quiet"])
        .costs("none")
        .args(extra)
        .output_dir(out)
        .ok()
}

/// Read one scalar out of a `metrics.yml` `run:` block. Returns `None` for a
/// key the document omits, which is how the optional fields read when absent.
fn run_field(metrics: &str, key: &str) -> Option<String> {
    let mut in_run = false;
    for line in metrics.lines() {
        if line.starts_with("run:") {
            in_run = true;
            continue;
        }
        if in_run {
            if !line.starts_with("  ") {
                break;
            }
            if let Some(rest) = line.trim().strip_prefix(&format!("{key}:")) {
                return Some(rest.trim().to_string());
            }
        }
    }
    None
}

#[track_caller]
fn field(metrics: &str, key: &str) -> String {
    run_field(metrics, key).unwrap_or_else(|| panic!("`run.{key}` missing from:\n{metrics}"))
}

/// Claim 1: `[from, until)` tiles. Splitting at a date must account for every
/// bar exactly once.
///
/// `--strict-from` on the second half so this measures the *slice* alone —
/// a warm-up read-back would legitimately re-read bars the first half
/// evaluated, which is a different property (claim 2).
#[test]
fn adjacent_ranges_partition_the_series() {
    let (_p, series) = scratch_file("daterange_tile.csv", &wavy_series(60));

    let whole = run_over(&series, &[], "dr_tile_whole").read("metrics.yml");
    let dev = run_over(&series, &["--until", "2024-02-01"], "dr_tile_dev").read("metrics.yml");
    let hold = run_over(
        &series,
        &["--from", "2024-02-01", "--strict-from"],
        "dr_tile_hold",
    )
    .read("metrics.yml");

    let bars = |m: &str| field(m, "bars").parse::<usize>().expect("bars is a count");
    assert_eq!(
        bars(&dev) + bars(&hold),
        bars(&whole),
        "the two halves must exactly cover the whole run"
    );
    assert_eq!(field(&dev, "period_start"), field(&whole, "period_start"));
    assert_eq!(field(&hold, "period_end"), field(&whole, "period_end"));
    // The boundary bar belongs to the second half, not the first.
    assert_eq!(field(&hold, "period_start"), "2024-02-01");
    assert!(field(&dev, "period_end").as_str() < "2024-02-01");
}

/// Claim 2: the read-back is **bounded**, so the answer does not depend on how
/// much history happens to precede the boundary.
///
/// Two files whose tails are bit-identical but whose heads differ by 150 bars,
/// sliced at the same `--from`. Both carry more than enough history to settle,
/// so a read-back capped at `stable_bars` reads the same bars in both and the
/// runs must agree exactly. Were the prefix instead "everything before
/// `--from`", the longer file would warm its EMAs 150 bars deeper and the two
/// would diverge.
#[test]
fn a_warmed_slice_does_not_depend_on_how_much_history_precedes_it() {
    let (_s, spec) = scratch_file("daterange_ema.yml", EMA_CROSS);
    let long = wavy_series(400);
    let short: String = {
        let mut lines = long.lines();
        let header = lines.next().expect("header");
        let kept: Vec<&str> = lines.skip(150).collect();
        format!("{header}\n{}\n", kept.join("\n"))
    };
    let (_a, long_arg) = scratch_file("daterange_long.csv", &long);
    let (_b, short_arg) = scratch_file("daterange_short.csv", &short);

    let from = ["--from", "2024-10-01"];
    let from_long = run_spec(&spec, &long_arg, &from, "dr_warm_long").read("metrics.yml");
    let from_short = run_spec(&spec, &short_arg, &from, "dr_warm_short").read("metrics.yml");

    assert_eq!(
        field(&from_long, "period_start"),
        field(&from_short, "period_start"),
        "both must begin evaluating on the same bar"
    );
    assert_eq!(
        field(&from_long, "warmup_bars"),
        field(&from_short, "warmup_bars"),
        "both must read back the same bounded depth"
    );
    assert_eq!(
        field(&from_long, "final_equity"),
        field(&from_short, "final_equity"),
        "a bounded read-back makes the two runs identical; they diverged, so \
         warm-up depth is leaking into the result"
    );
    assert_eq!(field(&from_long, "bars"), field(&from_short, "bars"));
}

/// The other half of claim 2: the read-back is not a no-op.
///
/// Same evaluated window, warmed versus `--strict-from` cold. An EMA carries
/// its history forever, so a run that settled 60 bars before the boundary must
/// reach a different equity than one that started the chain at it. If these
/// ever agree, the warm-up prefix is being fed but not actually advancing the
/// chains.
#[test]
fn warming_up_changes_the_answer() {
    let (_s, spec) = scratch_file("daterange_ema2.yml", EMA_CROSS);
    let (_p, series) = scratch_file("daterange_effect.csv", &wavy_series(300));

    let from = ["--from", "2024-07-01"];
    let warmed = run_spec(&spec, &series, &from, "dr_eff_warm").read("metrics.yml");
    let cold = run_spec(
        &spec,
        &series,
        &["--from", "2024-07-01", "--strict-from"],
        "dr_eff_cold",
    )
    .read("metrics.yml");

    assert_eq!(
        field(&warmed, "period_start"),
        field(&cold, "period_start"),
        "the two must measure the same window, or the comparison means nothing"
    );
    assert_ne!(
        field(&warmed, "final_equity"),
        field(&cold, "final_equity"),
        "warming an EMA before the boundary must change what it reads at it"
    );
}

/// The complement of claim 2: `--strict-from` deliberately does *not* warm, so
/// it may differ from the warmed run. Pinning that they are distinguishable
/// keeps the warm-up from being quietly a no-op — the failure mode the
/// invariance test above cannot see on its own.
#[test]
fn strict_from_starts_cold_and_reports_no_warmup() {
    let (_p, series) = scratch_file("daterange_strict.csv", &wavy_series(90));

    let warmed = run_over(&series, &["--from", "2024-03-01"], "dr_strict_warm").read("metrics.yml");
    let cold = run_over(
        &series,
        &["--from", "2024-03-01", "--strict-from"],
        "dr_strict_cold",
    )
    .read("metrics.yml");

    // Same evaluated window either way — the flag changes what was *read*.
    assert_eq!(field(&warmed, "period_start"), field(&cold, "period_start"));
    assert_eq!(field(&warmed, "period_end"), field(&cold, "period_end"));
    assert_eq!(field(&warmed, "bars"), field(&cold, "bars"));

    // Only the warmed run consumed a prefix.
    assert!(
        field(&warmed, "warmup_bars")
            .parse::<usize>()
            .expect("a count")
            > 0,
        "a warmed slice must report the prefix it consumed"
    );
    assert_eq!(
        run_field(&cold, "warmup_bars"),
        None,
        "a cold start has no warm-up prefix, so the key is omitted entirely"
    );
}

/// Claim 3, and the "warn and start late" branch: too little history before
/// `--from` to settle, so evaluation slips and the artifact records where it
/// actually began rather than where it was asked to.
#[test]
fn too_little_history_warns_and_starts_late() {
    let (_p, series) = scratch_file("daterange_short_hist.csv", &wavy_series(40));

    // The example strategy is an SMA(3)/SMA(8) crossover, so it needs ~9 bars.
    // Asking to start on the 3rd bar cannot be honoured.
    let out = Cmd::new("run")
        .arg(&at("examples/strategy.yml"))
        .series(&series)
        .args(&["--crypto", "-f", "1d", "--quiet", "--from", "2024-01-03"])
        .costs("none")
        .output_dir("dr_late")
        .ok();

    assert!(
        out.stderr.contains("only 2 bars precede"),
        "must say how much history it actually had:\n{}",
        out.stderr
    );

    let metrics = out.read("metrics.yml");
    assert!(
        field(&metrics, "period_start").as_str() > "2024-01-03",
        "evaluation started late, so period_start must say so, not echo --from"
    );
}

/// An unsliced run is unchanged: the three new fields describe the whole
/// series and no warm-up prefix is claimed.
#[test]
fn an_unsliced_run_reports_the_whole_series_and_no_warmup() {
    let metrics = run_over(&at("examples/candles.csv"), &[], "dr_plain").read("metrics.yml");
    assert_eq!(field(&metrics, "period_start"), "2024-01-01");
    assert_eq!(field(&metrics, "period_end"), "2024-01-30");
    assert_eq!(field(&metrics, "bars"), "30");
    assert_eq!(run_field(&metrics, "warmup_bars"), None);
}

/// `-w` windows tile from `--from`, not from the file start — otherwise a
/// windowed sliced run would report windows straddling the boundary.
#[test]
fn windows_tile_from_the_slice_not_the_file() {
    let (_p, series) = scratch_file("daterange_windowed.csv", &wavy_series(80));
    let out = run_over(
        &series,
        &["--from", "2024-02-01", "--strict-from", "-w", "10"],
        "dr_windows",
    );
    let rows = out.rows("metrics.csv");
    let first = rows.first().expect("at least one window row");
    assert!(
        first.starts_with("2024-02-01,"),
        "the first window must open on the --from bar, got: {first}"
    );
}

/// The three refusals. Each is a mistyped-input case that would otherwise
/// surface as a confusing empty or degenerate run.
#[test]
fn bad_ranges_are_refused_with_a_reason() {
    let series = at("examples/candles.csv");
    let cases: [(&[&str], &str); 4] = [
        (&["--from", "last tuesday"], "--from last tuesday"),
        (
            &["--from", "2024-01-10", "--until", "2024-01-10"],
            "half-open",
        ),
        (&["--from", "2030-01-01"], "select no bars"),
        (&["--strict-from"], "--strict-from"),
    ];
    for (flags, expect) in cases {
        let out = Cmd::new("run")
            .arg(&at("examples/strategy.yml"))
            .series(&series)
            .args(&["--crypto", "-f", "1d", "--quiet"])
            .costs("none")
            .args(flags)
            .output_dir("dr_bad")
            .run();
        assert!(!out.status.success(), "`{flags:?}` should have failed");
        let said = format!("{}{}", out.stderr, out.stdout);
        assert!(
            said.contains(expect),
            "`{flags:?}` should mention `{expect}`, said:\n{said}"
        );
    }
}

/// `--resume` continues from the state's last bar, so a `--from` at or before
/// it would re-run history rather than extend it.
#[test]
fn resuming_refuses_a_from_that_would_replay() {
    let (_p, series) = scratch_file("daterange_resume.csv", &wavy_series(60));
    let state = common::cli::unique_path("dr_state.json");
    let state_arg = state.to_str().expect("utf-8 path");

    // First leg: run the front of the series and save state.
    Cmd::new("run")
        .arg(&at("examples/strategy.yml"))
        .series(&series)
        .args(&["--crypto", "-f", "1d", "--quiet"])
        .costs("none")
        .args(&["--until", "2024-02-01", "--save-state", state_arg])
        .output_dir("dr_resume_first")
        .ok();

    // Second leg pointed back into bars the state already consumed.
    let out = Cmd::new("run")
        .arg(&at("examples/strategy.yml"))
        .series(&series)
        .args(&["--crypto", "-f", "1d", "--quiet"])
        .costs("none")
        .args(&["--from", "2024-01-10", "--resume", state_arg])
        .output_dir("dr_resume_bad")
        .run();

    assert!(!out.status.success(), "replaying history should be refused");
    let said = format!("{}{}", out.stderr, out.stdout);
    assert!(
        said.contains("re-run history"),
        "must explain why, said:\n{said}"
    );
}

/// `optimize` slices the same way `run` does, and says so on the same line.
#[test]
fn optimize_evaluates_only_the_sliced_range() {
    let (_p, series) = scratch_file("daterange_opt.csv", &wavy_series(80));
    let out = Cmd::new("optimize")
        .arg(&at("examples/strategy.yml"))
        .series(&series)
        .args(&["--crypto", "-f", "1d"])
        .costs("none")
        .args(&["--grid", "CASH=[1,2]", "--from", "2024-02-01"])
        .args(&["--output", "/dev/null"])
        .run();

    // The grid axis is inert for this spec; all that matters is that the
    // period line reports the sliced range with its warm-up called out.
    assert!(out.status.success(), "stderr:\n{}", out.stderr);
    assert!(
        out.stdout.contains("2024-02-01 →") && out.stdout.contains("warm-up"),
        "optimize should report the evaluated range and its warm-up:\n{}",
        out.stdout
    );
}
