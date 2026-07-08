// The whole test consumes `src/cli/metrics.rs` directly, so it only makes
// sense when the CLI feature is on.
#![cfg(feature = "cli")]

//! Cross-validation of fugazi's evaluation metrics against empyrical (with a
//! few manual numpy formulas for fields empyrical doesn't cover).
//!
//! This test consumes two committed CSVs from `tests/data/`:
//!   * `metrics_returns.csv`   — a deterministic 252-bar returns series (see
//!     `tools/gen_metrics_returns.py`, seeded so the file is reproducible).
//!   * `metrics_expected.csv`  — one `(metric, value)` row per reference
//!     figure, produced by `tools/gen_metrics_fixtures.py` (run once, needs
//!     empyrical installed).
//!
//! If the expected file is absent, the test **skips** (prints how to regenerate
//! it) so `cargo test` stays green without empyrical installed. When present,
//! fugazi is run over the identical returns and compared field-by-field.
//!
//! Constants (`INITIAL_CASH`, `BARS_PER_YEAR`, `RISK_FREE_RATE`) must match
//! `tools/gen_metrics_fixtures.py`.

#[path = "../src/cli/metrics.rs"]
#[allow(dead_code)]
mod metrics;

use std::collections::HashMap;
use std::path::PathBuf;

use fugazi::backtest::RunReport;
use fugazi::prelude::*;

const INITIAL_CASH: Real = 10_000.0;
const BARS_PER_YEAR: Real = 252.0;
const RISK_FREE_RATE: Real = 0.0;

/// Absolute tolerance for figures that match the reference *exactly* through
/// the same formula (means, stddev, VaR/CVaR, Sharpe/Sortino, max-DD, …). The
/// only slack is float rounding across the two implementations.
const EXACT_TOL: Real = 1e-9;

fn data_path(name: &str) -> PathBuf {
    [env!("CARGO_MANIFEST_DIR"), "tests", "data", name]
        .iter()
        .collect()
}

/// Load a two-column CSV `(header1,header2)`; returns `(header1_values,
/// header2_values)` as parallel `Vec<String>`s. No quoting/escaping — our
/// fixtures are plain numeric CSV.
fn read_two_col(path: &PathBuf) -> Option<(Vec<String>, Vec<String>)> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut lines = text.lines();
    let _header = lines.next()?;
    let mut ks = Vec::new();
    let mut vs = Vec::new();
    for line in lines.filter(|l| !l.trim().is_empty()) {
        let mut parts = line.splitn(2, ',');
        ks.push(parts.next()?.trim().to_string());
        vs.push(parts.next()?.trim().to_string());
    }
    Some((ks, vs))
}

fn synth_equity(returns: &[Real], cash: Real) -> Vec<Real> {
    let mut out = Vec::with_capacity(returns.len());
    let mut e = cash;
    for &r in returns {
        e *= 1.0 + r;
        out.push(e);
    }
    out
}

/// Pull a single scalar field out of the computed [`metrics::Metrics`] by its
/// dotted path (e.g. `"risk_adjusted.sharpe"`). Adds one arm per reference
/// field the generator writes; a missing arm surfaces the loud "unknown field"
/// message so a fixture drift is obvious.
fn field(m: &metrics::Metrics, key: &str) -> Real {
    let opt_or = |o: Option<Real>| o.expect("reference expects a defined value");
    match key {
        // RunSection
        "run.bars" => m.run.bars as Real,
        "run.initial_equity" => m.run.initial_equity,
        "run.final_equity" => m.run.final_equity,
        "run.bars_per_year" => m.run.bars_per_year,
        "run.risk_free_rate" => m.run.risk_free_rate,
        // ReturnSection
        "returns.total" => m.returns.total,
        "returns.total_pct" => m.returns.total_pct,
        "returns.cagr_pct" => opt_or(m.returns.cagr_pct),
        "returns.mean_bar" => m.returns.mean_bar,
        "returns.median_bar" => m.returns.median_bar,
        "returns.stddev_bar" => m.returns.stddev_bar,
        "returns.best_bar" => m.returns.best_bar,
        "returns.worst_bar" => m.returns.worst_bar,
        "returns.positive_bars_pct" => m.returns.positive_bars_pct,
        "returns.skewness" => opt_or(m.returns.skewness),
        "returns.kurtosis" => opt_or(m.returns.kurtosis),
        "returns.var_95" => m.returns.var_95,
        "returns.cvar_95" => m.returns.cvar_95,
        "returns.tail_ratio" => opt_or(m.returns.tail_ratio),
        "returns.annualized_mean_pct" => m.returns.annualized_mean_pct,
        "returns.annualized_volatility_pct" => m.returns.annualized_volatility_pct,
        // RiskAdjustedSection
        "risk_adjusted.sharpe" => opt_or(m.risk_adjusted.sharpe),
        "risk_adjusted.sortino" => opt_or(m.risk_adjusted.sortino),
        "risk_adjusted.calmar" => opt_or(m.risk_adjusted.calmar),
        "risk_adjusted.omega" => opt_or(m.risk_adjusted.omega),
        "risk_adjusted.ulcer_index" => m.risk_adjusted.ulcer_index,
        // DrawdownSection
        "drawdown.max" => m.drawdown.max,
        "drawdown.max_pct" => m.drawdown.max_pct,
        other => panic!("no accessor for reference field `{other}`"),
    }
}

#[test]
fn matches_empyrical_reference() {
    let returns_path = data_path("metrics_returns.csv");
    let expected_path = data_path("metrics_expected.csv");

    let (_return_indices, return_values) = match read_two_col(&returns_path) {
        Some(x) => x,
        None => panic!("missing {}: rerun tools/gen_metrics_returns.py", returns_path.display()),
    };
    let returns: Vec<Real> = return_values
        .iter()
        .map(|s| s.parse().expect("numeric return"))
        .collect();

    let (keys, values) = match read_two_col(&expected_path) {
        Some(x) => x,
        None => {
            eprintln!(
                "SKIP {}: run\n\
                 \n\
                 \tmamba env create -f tools/environment.yml   # or: conda\n\
                 \tmamba run -n fugazi-talib python3 tools/gen_metrics_fixtures.py\n\
                 \n\
                 to generate the empyrical reference values.",
                expected_path.display()
            );
            return;
        }
    };
    let expected: HashMap<String, Real> = keys
        .into_iter()
        .zip(values.into_iter().map(|s| s.parse().expect("numeric expected")))
        .collect();

    // Turn the returns into an equity curve (same shape a real backtest gives
    // us), then hand a synthetic RunReport to from_report() with the same
    // annualization + rf the generator used. No fills — this test targets
    // equity-curve-derived metrics only; trade-level metrics are covered by
    // unit tests.
    let equity = synth_equity(&returns, INITIAL_CASH);
    let report: RunReport<String> = RunReport {
        equity_curve: equity,
        fills: Vec::new(),
        initial_equity: INITIAL_CASH,
    };
    let m = metrics::from_report(&report, BARS_PER_YEAR, RISK_FREE_RATE, None);

    let mut mismatches: Vec<String> = Vec::new();
    for (key, &exp) in &expected {
        let got = field(&m, key);
        let diff = (got - exp).abs();
        // Scale-relative tolerance for large magnitudes (annualized pct
        // returns can be in the hundreds); absolute for values near zero.
        let tol = EXACT_TOL.max(exp.abs() * 1e-9);
        if diff > tol {
            mismatches.push(format!(
                "{key}: got {got}, expected {exp}, diff {diff} (tol {tol})"
            ));
        }
    }

    assert!(
        mismatches.is_empty(),
        "empyrical-reference divergence:\n  {}",
        mismatches.join("\n  ")
    );
}
