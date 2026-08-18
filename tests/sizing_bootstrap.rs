//! The two self-referential sizers have to be able to start.
//!
//! `!equity_vol_target` and `!fractional_kelly` size on something that only
//! exists *because* the strategy traded — a moving equity curve, and closed
//! trades. Everywhere else in fugazi an unready source reads `None` and the
//! strategy waits, which is the right safe default; but a sizing slot reading
//! `None` **skips the trade**, so here waiting never ends. No entry ⇒ no trade
//! ⇒ no sample ⇒ no entry.
//!
//! Both reported zero fills on every shape, with no warning and no error — the
//! run simply did nothing. `seed:` is the size until the recipe can answer for
//! itself, and these pin that the bootstrap actually works end to end rather
//! than only in the indicator's unit tests.

mod common;

use common::cli::{Cmd, scratch_file};

/// A crossover that trades readily, so the only thing under test is whether
/// the sizer lets it.
fn strategy_with(sizing: &str) -> String {
    format!(
        "\
symbol: BTCUSDT
long:
  enter: !crosses_above {{ lhs: !sma {{ source: close, period: 5 }}, rhs: !sma {{ source: close, period: 20 }} }}
  exit: !crosses_below {{ lhs: !sma {{ source: close, period: 5 }}, rhs: !sma {{ source: close, period: 20 }} }}
sizing: {sizing}
"
    )
}

/// An oscillating trend, so the crossover fires repeatedly.
fn series() -> String {
    let mut out = String::from("symbol;freq;time;open;high;low;close;volume\n");
    for d in 0..400 {
        // 2022-01-01 + d days, spelled out rather than date arithmetic.
        let day = 1 + d % 28;
        let month = 1 + (d / 28) % 12;
        let year = 2022 + d / 336;
        let p = 100.0 + 20.0 * ((d as f64) / 11.0).sin() + (d as f64) * 0.05;
        out += &format!(
            "BTCUSDT;1d;{year}-{month:02}-{day:02}T00:00:00Z;{p:.2};{:.2};{:.2};{:.2};1000\n",
            p + 1.0,
            p - 1.0,
            p + 0.3,
        );
    }
    out
}

fn fills_for(name: &str, sizing: &str) -> usize {
    let (_, spec) = scratch_file(&format!("{name}.yml"), &strategy_with(sizing));
    let (_, csv) = scratch_file(&format!("{name}.csv"), &series());
    let out = Cmd::new("run")
        .arg(&spec)
        .series(&csv)
        .args(&["--crypto", "-f", "1d", "-c", "100000", "--quiet"])
        .costs("none")
        .output_dir(name)
        .ok();
    out.read("metrics.yml")
        .lines()
        .find_map(|l| l.trim().strip_prefix("total_fills:"))
        .map(|v| v.trim().parse().expect("total_fills is a count"))
        .expect("metrics.yml carries total_fills")
}

#[test]
fn equity_vol_target_bootstraps_from_a_flat_curve() {
    // A flat equity curve has exactly zero volatility, and `div` reads `None`
    // on a zero denominator — so this used to be 0.
    assert!(
        fills_for(
            "sizing_equity_vol",
            "!equity_vol_target { target: 0.20, window: 30, bars_per_year: 365 }",
        ) > 0,
        "equity_vol_target never opened a position, so it can never acquire \
         the equity variance it sizes on"
    );
}

#[test]
fn fractional_kelly_bootstraps_from_no_closed_trades() {
    assert!(
        fills_for(
            "sizing_kelly",
            "!fractional_kelly { kelly_fraction: 0.5, window: 30 }",
        ) > 0,
        "fractional_kelly never opened a position, so no trade can ever close \
         to fill its window"
    );
}

/// The bootstrap is the seed doing the work, not something incidental: asking
/// for a zero seed reproduces the old deadlock exactly. This is what keeps the
/// two tests above honest if the strategy or the series is ever retuned.
#[test]
fn a_zero_seed_still_deadlocks_which_is_what_makes_the_seed_the_fix() {
    assert_eq!(
        fills_for(
            "sizing_kelly_zero_seed",
            "!fractional_kelly { kelly_fraction: 0.5, window: 30, seed: 0.0 }",
        ),
        0,
        "a zero seed should still skip every entry — if this trades, the tests \
         above are not measuring the seed"
    );
}

/// The sizer that already worked has to keep working, and at the same size —
/// the seed must not have quietly changed anything with a settled reading.
#[test]
fn drawdown_throttle_is_unchanged() {
    let baseline = fills_for("sizing_baseline", "!value 1.0");
    assert_eq!(
        fills_for("sizing_throttle", "!drawdown_throttle { max_drawdown: 0.20 }"),
        baseline,
        "drawdown_throttle reads a drawdown that is well-defined at zero, so \
         it never needed a seed and must be untouched"
    );
}
