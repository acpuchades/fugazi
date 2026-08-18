// Reads `fugazi::spec::metrics`, so it needs the `spec` feature (which the
// default `cli` enables).
#![cfg(feature = "spec")]

//! Every metric is cross-validated, or explicitly exempted with a reason.
//!
//! The three metric-facing cross-checks each compare only the fields their
//! generator happens to write. Nothing made them *complete*: adding a field to
//! [`metrics::Metrics`] and forgetting to add it to a generator left the field
//! with no external reference at all, and no test went red to say so. That is
//! the same silent-rot failure the skip-vs-fail policy exists to prevent
//! (`tests/common/fixtures.rs`), one level up — there, a whole suite could
//! disable itself; here, a single field could quietly opt out.
//!
//! So this file inverts the direction. It walks
//! [`metrics::flatten`] — the authoritative field list, which the compiler
//! forces you to extend when you add a metric — and asserts that each key is
//! either present in one of the committed reference fixtures or named in
//! [`EXEMPT`] with a reason. A new metric fails here until you have done one or
//! the other.
//!
//! It is deliberately **not** skippable. It reads the fixtures for their key
//! sets, never their values, so it needs no reference library and cannot rot
//! the way the suites it guards can.

mod common;

use std::collections::HashSet;

use fugazi::spec::metrics;

use common::fixtures::Csv;

/// Fields with no external reference, and why. Every entry is a claim that the
/// field is checked *somewhere* — the unit tests in `src/metrics.rs`, or the
/// suite named — and that no reference library offers a second opinion on it.
///
/// Shrinking this list is the point. Adding to it is a decision, not a
/// formality: prefer finding a reference over writing a line here.
const EXEMPT: &[(&str, &str)] = &[
    // Run context that describes the slice rather than measuring it.
    (
        "run.warmup_bars",
        "bookkeeping, not a measurement: how many bars `--from` read back to \
         warm the chains before evaluation began. No reference library models \
         a warm-up prefix at all. Covered by tests/date_range.rs",
    ),
    // Trade fields no reference library reports.
    (
        "trades.flat",
        "exactly-zero-PnL round trips; backtesting.py buckets these as losses, \
         so there is nothing to compare against. Unit-tested in src/metrics.rs",
    ),
    (
        "trades.max_consecutive_wins",
        "backtesting.py reports no streak statistics. Unit-tested in \
         src/metrics.rs::streaks_track_longest_run",
    ),
    (
        "trades.max_consecutive_losses",
        "as trades.max_consecutive_wins",
    ),
    (
        "trades.average_seconds",
        "`average_bars * seconds_per_bar`, and average_bars is cross-checked; \
         the multiplication is a calendar concern, covered by src/spec/calendar.rs",
    ),
    ("trades.min_seconds", "as trades.average_seconds"),
    ("trades.max_seconds", "as trades.average_seconds"),
    // Risk-adjusted fields empyrical has no equivalent for.
    (
        "risk_adjusted.probabilistic_sharpe",
        "Bailey & López de Prado's PSR; empyrical implements no such statistic. \
         Unit-tested in src/metrics.rs against a hand-derived Φ evaluation",
    ),
    (
        "risk_adjusted.ulcer_performance_index",
        "`(CAGR - rf) / ulcer_index`; both operands are cross-checked by \
         metrics_validation.rs, the quotient is not separately referenced",
    ),
    (
        "drawdown.recovery_factor",
        "`total_return / max_drawdown`; both operands are cross-checked by \
         metrics_validation.rs, the quotient is not separately referenced",
    ),
    // Cost aggregates. The cost *pipeline* is cross-checked one leg at a time
    // by wallet_validation.rs against vectorbt; these are run-level roll-ups of
    // it, and the gross-vs-net pairing that derives slippage cost has no
    // counterpart in any reference library.
    (
        "costs.total_commission",
        "roll-up of fills the wallet booked; the commission model itself is \
         cross-checked by wallet_validation.rs",
    ),
    (
        "costs.total_slippage_cost",
        "derived by pairing net fills against a gross re-run — no reference \
         library models costs that way. Covered by tests/costs.rs",
    ),
    ("costs.cost_drag_pct", "as costs.total_slippage_cost"),
];

/// The `(metric, expected)` fixtures, by the suite that owns each.
const FIXTURES: [(&str, &str); 2] = [
    ("metrics_expected.csv", "metrics_validation.rs (empyrical)"),
    (
        "trade_metrics_expected.csv",
        "trade_metrics_validation.rs (backtesting.py)",
    ),
];

/// Keys carrying a reference value in any committed fixture.
///
/// A fixture that is absent contributes nothing rather than failing the run:
/// this test guards *completeness of intent*, and a contributor who deleted a
/// generated CSV should be told about it by the suite that owns it, not here.
/// The assertion below still holds, because a missing fixture can only make the
/// covered set smaller and so can only turn this test red.
fn referenced_keys() -> HashSet<String> {
    FIXTURES
        .iter()
        .filter_map(|(file, _)| Csv::load(file))
        .flat_map(|csv| csv.strings("metric"))
        .collect()
}

#[test]
fn every_metric_is_cross_validated_or_exempt() {
    // `flatten` needs a document; its *values* are irrelevant here, only its
    // key set, which is fixed.
    let empty = fugazi::backtest::RunReport::<&'static str> {
        equity_curve: vec![100.0, 101.0, 99.0],
        fills: Vec::new(),
        rejections: Vec::new(),
        initial_equity: 100.0,
    };
    let sample = metrics::from_report(&empty, 252.0, 0.0, None);

    let referenced = referenced_keys();
    let exempt: HashSet<&str> = EXEMPT.iter().map(|(k, _)| *k).collect();

    let uncovered: Vec<&str> = metrics::flatten(&sample)
        .into_iter()
        .map(|(k, _)| k)
        .filter(|k| !referenced.contains(*k) && !exempt.contains(k))
        .collect();

    assert!(
        uncovered.is_empty(),
        "{} metric(s) have no external reference and no exemption:\n  {}\n\n\
         Add a reference value in one of:\n  {}\n\
         (then `pixi run gen`), or add the field to EXEMPT in this file with \
         the reason no reference library covers it.",
        uncovered.len(),
        uncovered.join("\n  "),
        FIXTURES
            .iter()
            .map(|(f, owner)| format!("tools/ -> tests/data/{f}  — {owner}"))
            .collect::<Vec<_>>()
            .join("\n  "),
    );
}

/// An exemption for a field that no longer exists is dead weight that reads as
/// coverage. It also catches a rename: the old name lingers here while the new
/// one is uncovered, and without this the first failure would be confusing.
#[test]
fn no_exemption_names_a_field_that_no_longer_exists() {
    let empty = fugazi::backtest::RunReport::<&'static str> {
        equity_curve: vec![100.0],
        fills: Vec::new(),
        rejections: Vec::new(),
        initial_equity: 100.0,
    };
    let sample = metrics::from_report(&empty, 252.0, 0.0, None);
    let known: HashSet<&str> = metrics::flatten(&sample).into_iter().map(|(k, _)| k).collect();

    let stale: Vec<&str> = EXEMPT
        .iter()
        .map(|(k, _)| *k)
        .filter(|k| !known.contains(k))
        .collect();

    assert!(
        stale.is_empty(),
        "EXEMPT names {} field(s) that metrics::flatten no longer emits — \
         renamed or removed:\n  {}",
        stale.len(),
        stale.join("\n  ")
    );
}

/// A field cannot be both exempt and referenced: whichever the author meant,
/// the other is misleading. Usually it means a reference was added later and
/// the exemption was not removed, so the reason still claims none exists.
#[test]
fn no_exemption_duplicates_a_reference() {
    let referenced = referenced_keys();
    // Nothing to say when the fixtures aren't present — see `referenced_keys`.
    if referenced.is_empty() {
        return;
    }
    let both: Vec<&str> = EXEMPT
        .iter()
        .map(|(k, _)| *k)
        .filter(|k| referenced.contains(*k))
        .collect();

    assert!(
        both.is_empty(),
        "{} field(s) are exempt *and* carry a reference value — drop the \
         exemption, its stated reason is no longer true:\n  {}",
        both.len(),
        both.join("\n  ")
    );
}
