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
root: BTCUSDT
long:
  enter: !gt { lhs: !close, rhs: !param { key: LEVEL } }
  exit: !lt { lhs: !close, rhs: !param { key: LEVEL } }
sizing: !value 1.0
",
    );
    let out = Cmd::new("check").arg("strategy").arg(&spec).ok();
    assert!(out.stdout.contains("ok"), "{}", out.stdout);
}

/// A `!match` pattern is a `!value` literal, not a node — the other hand-rolled
/// parse, which used to leak the internal hole sentinel into its error message.
const MATCH_PARAM_PATTERN: &str = "\
long:
  enter: !gt
    lhs: !match
      on: !close
      cases:
        - when: !param LEVEL
          value: !value 1
      default: !value 0
    rhs: !value 0
";

/// A `!param` standing in for a *whole expression* is a missing value like any
/// other — `check` must validate around it, name it, and leave the demand for
/// `run`.
///
/// Regression: an expression-slot hole used to parse as a `!value 0.0`
/// constant, which claims a type the placeholder has not chosen yet. In a Bool
/// slot the type check then rejected the document — "`!value` produces Real,
/// but a Bool-valued expression is required here" — a document error for a
/// document whose only gap was a `--params` value. In a Real slot it parsed,
/// but the hole was recorded nowhere (this parse is hand-rolled, so nothing
/// answered a `deserialize_*` call at it), so `check` reported no placeholder
/// at all and the run failed on a value the report never asked for.
#[test]
fn check_validates_around_a_param_standing_for_a_whole_expression() {
    for (name, doc, param) in [
        // Bool slot: `enter:` demands a signal.
        ("bool", "long:\n  enter: !param SIGNAL\n", "SIGNAL"),
        // Real slot: parses either way, but has to be *reported*.
        (
            "real",
            "long:\n  enter: !gt { lhs: !close, rhs: !param LEVEL }\n",
            "LEVEL",
        ),
        ("literal", MATCH_PARAM_PATTERN, "LEVEL"),
    ] {
        let (_, spec) = scratch_file(
            &format!("expr_param_{name}.yml"),
            &format!("root: BTCUSDT\n{doc}"),
        );
        let out = Cmd::new("check").arg("strategy").arg(&spec).ok();
        assert!(
            out.stdout.contains("1 unset placeholder"),
            "{name}: the placeholder must be counted: {}",
            out.stdout
        );
        assert!(
            out.stdout
                .contains(&format!("--params {param}=<expression>")),
            "{name}: and named, with the shape it needs: {}",
            out.stdout
        );
    }
}

/// The same document *run* still errors: `check` validates around a hole, but
/// nothing runs on one.
#[test]
fn an_expression_param_still_fails_a_run() {
    let (_s, series) = scratch_file(
        "expr_param.csv",
        "time,symbol,open,high,low,close,volume\n         2024-01-01T00:00:00Z,BTCUSDT,100,101,99,100,1000\n         2024-01-02T00:00:00Z,BTCUSDT,100,102,99,101,1000\n",
    );
    let (_d, doc) = scratch_file(
        "expr_param_run.yml",
        "root: BTCUSDT\nlong:\n  enter: !param SIGNAL\n",
    );
    let out = Cmd::new("run")
        .arg(&doc)
        .series(&series)
        .args(&["--crypto", "--quiet"])
        .output_dir("check_builds_expr_param")
        .fails();
    assert!(
        out.stderr.contains("parameter `SIGNAL` is not set"),
        "{}",
        out.stderr
    );
}

/// `!get` resolves against an overlay schema that only real `--series` data
/// carries. `check` has none, so building would reject every legitimate
/// overlay column as unknown.
#[test]
fn check_still_skips_the_build_for_an_overlay_column() {
    let (_, spec) = scratch_file(
        "overlayed.yml",
        "\
root: BTCUSDT
long:
  enter: !gt { lhs: !get { key: funding_rate }, rhs: !value 0.0 }
  exit: !lt { lhs: !get { key: funding_rate }, rhs: !value 0.0 }
sizing: !value 1.0
",
    );
    let out = Cmd::new("check").arg("strategy").arg(&spec).ok();
    assert!(out.stdout.contains("ok"), "{}", out.stdout);
}

/// A portfolio's `weights:` expression is read only inside a rebalance cycle.
/// Omit `rebalance_on:` and the expression is built, updated every bar, and
/// consulted on none — the portfolio silently runs its equal-split seed. The
/// document parses fine (every field is well-typed), so the build is the only
/// place this can be caught, which is exactly what `check` reaches for here.
const PORTFOLIO_INERT_WEIGHTS: &str = "\
weights: !drawdown_throttle { source: !portfolio_book, max_drawdown: 0.15 }
children:
  - strategy: !buy_and_hold { root: BTCUSDT }
  - strategy: !buy_and_hold { root: ETHUSDT }
";

/// The same document with a cadence, so the weights actually get applied.
const PORTFOLIO_LIVE_WEIGHTS: &str = "\
weights: !drawdown_throttle { source: !portfolio_book, max_drawdown: 0.15 }
rebalance_on: !every 28
children:
  - strategy: !buy_and_hold { root: BTCUSDT }
  - strategy: !buy_and_hold { root: ETHUSDT }
";

#[test]
fn check_rejects_a_portfolio_whose_weights_can_never_be_read() {
    let (_, spec) = scratch_file("portfolio_inert_weights.yml", PORTFOLIO_INERT_WEIGHTS);
    let out = Cmd::new("check")
        .arg("strategy")
        .arg(&format!("portfolio:{spec}"))
        .fails();

    assert!(
        out.stderr.contains("weights:"),
        "expected the inert-weights error:\n{}",
        out.stderr
    );
    // Without this the user is told the document is wrong but not which field
    // makes it right again.
    assert!(
        out.stderr.contains("rebalance_on:"),
        "error did not name the field that fixes it:\n{}",
        out.stderr
    );
}

#[test]
fn check_accepts_the_same_portfolio_once_it_says_when_to_rebalance() {
    let (_, spec) = scratch_file("portfolio_live_weights.yml", PORTFOLIO_LIVE_WEIGHTS);
    let out = Cmd::new("check")
        .arg("strategy")
        .arg(&format!("portfolio:{spec}"))
        .ok();
    assert!(out.stdout.contains("ok"), "{}", out.stdout);
}
