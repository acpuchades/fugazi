//! `check` validates shape, not values — but for a document that is *fully
//! determined* it now also builds, which catches a class of error the typed
//! parse structurally cannot.
//!
//! The motivating case: a `pairs:` document sizing on `!vol_target`. A pair
//! holds two assets and privileges neither, so a leaf with no `source:` has no
//! asset to read; that used to build a sole-atom `Pick` and panic mid-run,
//! *after* `check` had reported `ok`, so there was no way to find out ahead of
//! the run. `!vol_target` reads prices while looking like a scalar knob, which
//! is why it was the one people hit.
//!
//! Building here is conditional, and the conditions are the point — a hole has
//! no value to build from, and `!get` needs an overlay schema only real data
//! supplies. The skip tests below pin both, because a `check` that started
//! failing on those would be far worse than the panic it replaced.

mod common;

use common::cli::{Cmd, scratch_file};

/// `!vol_target` reads prices, so on a pair it has no asset to measure.
const PAIRS_VOL_TARGET: &str = "\
left: BTCUSDT
right: ETHUSDT
long_spread:
  enter: !gt { lhs: !close { source: !pick { symbol: BTCUSDT } }, rhs: !value 0.0 }
  exit: !lt { lhs: !close { source: !pick { symbol: BTCUSDT } }, rhs: !value 0.0 }
sizing: !vol_target { target: 0.20, window: 30, bars_per_year: 365 }
";

/// The same document with the sizer told which leg to measure.
const PAIRS_ROOTED: &str = "\
left: BTCUSDT
right: ETHUSDT
long_spread:
  enter: !gt { lhs: !close { source: !pick { symbol: BTCUSDT } }, rhs: !value 0.0 }
  exit: !lt { lhs: !close { source: !pick { symbol: BTCUSDT } }, rhs: !value 0.0 }
sizing: !vol_target
  source: !pick { symbol: BTCUSDT }
  target: 0.20
  window: 30
  bars_per_year: 365
";

#[test]
fn check_rejects_a_pair_whose_sizer_names_no_asset() {
    let (_, spec) = scratch_file("pairs_vol_target.yml", PAIRS_VOL_TARGET);
    let out = Cmd::new("check")
        .arg("strategy")
        .arg(&format!("pairs:{spec}"))
        .fails();

    assert!(
        out.stderr.contains("ambiguous"),
        "expected the ambiguity error:\n{}",
        out.stderr
    );
    // Without the breadcrumb the user is told a document is ambiguous but not
    // which of its knobs made it so.
    assert!(
        out.stderr.contains("!vol_target"),
        "error did not name the offending tag:\n{}",
        out.stderr
    );
}

#[test]
fn check_accepts_the_same_pair_once_the_sizer_names_its_leg() {
    let (_, spec) = scratch_file("pairs_rooted.yml", PAIRS_ROOTED);
    let out = Cmd::new("check")
        .arg("strategy")
        .arg(&format!("pairs:{spec}"))
        .ok();
    assert!(out.stdout.contains("ok"), "{}", out.stdout);
}

/// An unresolved `!param` is a *hole*: `check`'s whole point is to validate
/// such a document without the user supplying values, so it must not try to
/// build one.
#[test]
fn check_still_skips_the_build_when_a_param_is_unresolved() {
    let (_, spec) = scratch_file(
        "holey.yml",
        "\
symbol: BTCUSDT
long:
  enter: !gt { lhs: !close, rhs: !param { key: LEVEL } }
  exit: !lt { lhs: !close, rhs: !param { key: LEVEL } }
sizing: !value 1.0
",
    );
    let out = Cmd::new("check").arg("strategy").arg(&spec).ok();
    assert!(out.stdout.contains("ok"), "{}", out.stdout);
}

/// `!get` resolves against an overlay schema that only real `--series` data
/// carries. `check` has none, so building would reject every legitimate
/// overlay column as unknown.
#[test]
fn check_still_skips_the_build_for_an_overlay_column() {
    let (_, spec) = scratch_file(
        "overlayed.yml",
        "\
symbol: BTCUSDT
long:
  enter: !gt { lhs: !get { key: funding_rate }, rhs: !value 0.0 }
  exit: !lt { lhs: !get { key: funding_rate }, rhs: !value 0.0 }
sizing: !value 1.0
",
    );
    let out = Cmd::new("check").arg("strategy").arg(&spec).ok();
    assert!(out.stdout.contains("ok"), "{}", out.stdout);
}
