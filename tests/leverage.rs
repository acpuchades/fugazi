//! One `sizing:` value has to mean one exposure, whichever side the document
//! takes — and what the wallet could not honour has to be findable afterwards.
//!
//! `Size::value_frac` resolves to a magnitude and the side comes from the leg,
//! so `sizing: 3.0` reads as "three times equity" on both. It did not behave
//! that way. A buy was bounded by the cash it spent, so the long leg was
//! quietly scaled back to 1x; a sale *credits* cash, so nothing bounded the
//! short and it took the full 3x. A long/short document therefore reported a
//! curve describing neither leg, and a long-only document asking for 10x
//! backtested as 1x with no way to find out at any layer — the fill reported
//! the fitted size as if it were what was asked for, and nothing landed in
//! `rejections`, because a scaled fill is not a refusal.
//!
//! What bounds both sides is gross notional (`PaperWallet::with_max_gross`), and
//! what makes the gap visible is `Order::requested_units`. These drive both
//! through the CLI, which is the layer a user would have hit it at.

mod common;

use common::cli::{Cmd, Outcome, scratch_file};

/// Enter on the first bar and never leave, so the only thing under test is how
/// large the position the wallet ends up carrying is.
fn document(side: &str, sizing: &str) -> String {
    format!(
        "\
root: S
{side}:
  enter: !gt {{ lhs: !close, rhs: 0 }}
sizing: {sizing}
"
    )
}

/// A flat series: every bar opens, trades and closes at 100, so a position's
/// notional is its unit count times 100 and nothing moves under the test.
fn series() -> String {
    let mut out = String::from("symbol;freq;time;open;high;low;close;volume\n");
    for d in 0..6 {
        out += &format!("S;1d;2024-01-{:02}T00:00:00Z;100;100;100;100;1000\n", d + 1);
    }
    out
}

struct Run {
    out: Outcome,
}

impl Run {
    /// The single fill's `(units, requested_units)`.
    fn fill(&self) -> (f64, f64) {
        let rows = self.out.rows("fills.csv");
        assert_eq!(rows.len(), 1, "expected one fill, got:\n{rows:?}");
        let cols: Vec<&str> = rows[0].split(',').collect();
        (
            cols[3].parse().expect("units"),
            cols[4].parse().expect("requested_units"),
        )
    }

    /// Gross notional as a multiple of the account's starting equity.
    fn gross_over_equity(&self) -> f64 {
        let (units, _) = self.fill();
        units.abs() * 100.0 / 10_000.0
    }
}

fn run(name: &str, side: &str, sizing: &str, extra: &[&str]) -> Run {
    let (_, spec) = scratch_file(&format!("{name}.yml"), &document(side, sizing));
    let (_, csv) = scratch_file(&format!("{name}.csv"), &series());
    let mut args = vec!["--crypto", "-f", "1d", "-c", "10000"];
    args.extend_from_slice(extra);
    let out = Cmd::new("run")
        .arg(&spec)
        .series(&csv)
        .args(&args)
        .costs("none")
        .output_dir(name)
        .ok();
    Run { out }
}

/// The bug, at the layer it was reported from: same document, same account,
/// same bars, only the side differs.
#[test]
fn one_sizing_value_means_one_exposure_on_both_sides() {
    let long = run("lev_long_3x", "long", "3.0", &["--quiet"]);
    let short = run("lev_short_3x", "short", "3.0", &["--quiet"]);

    assert!(
        (long.gross_over_equity() - short.gross_over_equity()).abs() < 1e-9,
        "long took {:.2}x and short took {:.2}x under one spec value",
        long.gross_over_equity(),
        short.gross_over_equity(),
    );
    assert!(
        (long.gross_over_equity() - 1.0).abs() < 1e-9,
        "an unlevered account should carry 1x, got {:.2}x",
        long.gross_over_equity(),
    );
}

/// A request the account cannot carry is not silently reinterpreted: the fill
/// says what was asked for beside what was traded, on both sides.
#[test]
fn a_fitted_fill_records_the_size_that_was_asked_for() {
    for (name, side) in [("lev_ask_long", "long"), ("lev_ask_short", "short")] {
        let r = run(name, side, "10.0", &["--quiet"]);
        let (units, requested) = r.fill();
        assert!((units.abs() - 100.0).abs() < 1e-9, "{name}: units {units}");
        assert!(
            (requested.abs() - 1_000.0).abs() < 1e-9,
            "{name}: the document asked for 10x and the blotter says {requested}",
        );
    }
}

/// ...and the run says so out loud rather than leaving it in a column nobody
/// reads. A 10x request filled at 1x is 10% of the ask, well past the 1% slack
/// an all-in needs for commission.
#[test]
fn a_materially_fitted_run_warns() {
    let r = run("lev_warn", "long", "10.0", &[]);
    assert!(
        r.out.stdout.contains("scaled down to fit the account"),
        "no banner for a request filled at a tenth of its size:\n{}",
        r.out.stdout,
    );
    // A run that asked for what it could have stays quiet — the threshold is
    // there because *any* reduction is otherwise true on every costed all-in.
    let quiet = run("lev_no_warn", "long", "1.0", &[]);
    assert!(
        !quiet.out.stdout.contains("scaled down to fit the account"),
        "an ordinary all-in should not warn:\n{}",
        quiet.out.stdout,
    );
}

/// The knob that makes a levered backtest possible at all, and the reason the
/// bound is expressed as leverage rather than hard-wired: `--max-gross 3`
/// honours the same document in full, on both sides.
#[test]
fn raising_the_cap_honours_the_request_on_both_sides() {
    for (name, side) in [("lev_3x_long", "long"), ("lev_3x_short", "short")] {
        let r = run(name, side, "3.0", &["--quiet", "--max-gross", "3"]);
        let (units, requested) = r.fill();
        assert!(
            (units.abs() - 300.0).abs() < 1e-9,
            "{name}: expected the full 3x, got {units} units",
        );
        assert!(
            (units - requested).abs() < 1e-9,
            "{name}: nothing should have been fitted, {units} vs {requested}",
        );
        assert!(
            (r.gross_over_equity() - 3.0).abs() < 1e-9,
            "{name}: {:.2}x",
            r.gross_over_equity(),
        );
    }
}
