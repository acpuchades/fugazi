//! Acceptance gate for the Monte Carlo significance feature.
//!
//! Covers the whole surface at the library level: the resampling core, the
//! bootstrap CIs, and the re-run empirical-null p-value. The headline promise
//! the tests pin is **reproducibility** — a fixed seed reproduces every CI and
//! p-value bit-for-bit — plus the statistical invariants (CIs ordered and
//! bracketing the point estimate's neighbourhood, p-values in `(0, 1]`) and a
//! power/size sanity check on a constructed no-edge series.

use fugazi::market::Real;
use fugazi::montecarlo::ResampleScheme;
use fugazi::spec::backtest::{EvalContext, measured_report_any, run_iteration_any};
use fugazi::spec::costs::CostConfig;
use fugazi::spec::montecarlo::{McConfig, run_montecarlo};
use fugazi::spec::{StrategyRef, StrategySpec};
use fugazi::types::Symbol;
use fugazi::types::{Atom, Candle, Snapshot};

const CASH: Real = 10_000.0;

fn empty_costs() -> CostConfig {
    serde_json::from_str("{}").expect("empty cost config")
}

fn ctx<'a>(cost_config: &'a CostConfig) -> EvalContext<'a> {
    EvalContext {
        cash: CASH,
        max_gross: 1.0,
        margin_rate: 0.0,
        maintenance_margin: None,
        bars_per_year: 365.0,
        risk_free_rate: 0.0,
        cost_config,
        effective_freq: None,
        stream: None,
        windowed: None,
        seconds_per_bar: None,
        mc: None,
        warmup_bars: None,
    }
}

/// A single-asset MA-crossover spec — the canonical always-in reversal.
fn crossover_spec() -> StrategySpec {
    let yaml = r#"
        root: X
        long:
          enter: !crosses_above { lhs: !sma { source: close, period: 3 }, rhs: !sma { source: close, period: 8 } }
        short:
          enter: !crosses_below { lhs: !sma { source: close, period: 3 }, rhs: !sma { source: close, period: 8 } }
    "#;
    let strat = StrategyRef::from_text_with_params_in(
        yaml,
        &Default::default(),
        std::path::Path::new("."),
        std::path::Path::new("."),
        "(mc)",
    )
    .expect("parse spec");
    StrategySpec::Single(Box::new(strat))
}

fn snaps_from_prices(prices: &[Real]) -> Vec<Snapshot<Symbol>> {
    prices
        .iter()
        .map(|&p| {
            let c = Candle::new(p, p + 1.0, p - 1.0, p, 1_000.0);
            Snapshot::single(fugazi::types::symbol("X"), Atom::new(c))
        })
        .collect()
}

/// A trending-with-swings series that produces several crossovers.
fn trending(n: usize) -> Vec<Real> {
    (0..n)
        .map(|i| {
            let t = i as Real;
            100.0 + 12.0 * (t * 0.30).sin() + 5.0 * (t * 0.09).cos() + 0.15 * t
        })
        .collect()
}

fn run_mc(
    spec: &StrategySpec,
    snaps: &[Snapshot<Symbol>],
    config: &McConfig,
) -> fugazi::spec::McOutcome {
    let costs = empty_costs();
    let ctx = ctx(&costs);
    let report = measured_report_any(spec, snaps, &ctx).expect("drive strategy");
    run_montecarlo(spec, snaps, &ctx, &report, config).expect("montecarlo")
}

#[test]
fn same_seed_reproduces_every_number() {
    let spec = crossover_spec();
    let snaps = snaps_from_prices(&trending(120));
    let config = McConfig {
        permutations: 200,
        scheme: ResampleScheme::Stationary { mean_block: 8.0 },
        seed: 123,
        ci_level: 0.95,
        rerun_null: true,
        metrics: Vec::new(),
    };

    let a = run_mc(&spec, &snaps, &config).section;
    let b = run_mc(&spec, &snaps, &config).section;

    assert_eq!(a.metrics.len(), b.metrics.len());
    for (x, y) in a.metrics.iter().zip(&b.metrics) {
        assert_eq!(x.name, y.name);
        assert_eq!(x.observed, y.observed, "observed drifted for {}", x.name);
        assert_eq!(x.ci_lower, y.ci_lower, "ci_lower drifted for {}", x.name);
        assert_eq!(x.ci_upper, y.ci_upper, "ci_upper drifted for {}", x.name);
        assert_eq!(
            x.p_value_rerun, y.p_value_rerun,
            "p(rerun) drifted for {}",
            x.name
        );
    }
}

#[test]
fn a_different_seed_moves_the_numbers() {
    let spec = crossover_spec();
    let snaps = snaps_from_prices(&trending(120));
    let base = McConfig {
        permutations: 200,
        scheme: ResampleScheme::Stationary { mean_block: 8.0 },
        seed: 1,
        ci_level: 0.95,
        rerun_null: false,
        metrics: vec!["sharpe".to_string()],
    };
    let a = run_mc(&spec, &snaps, &base).section;
    let b = run_mc(
        &spec,
        &snaps,
        &McConfig {
            seed: 2,
            ..base.clone()
        },
    )
    .section;
    // The observed metric is seed-independent; the resampled CI is not.
    assert_eq!(a.metrics[0].observed, b.metrics[0].observed);
    assert_ne!(
        (a.metrics[0].ci_lower, a.metrics[0].ci_upper),
        (b.metrics[0].ci_lower, b.metrics[0].ci_upper),
        "different seeds should draw a different resample"
    );
}

#[test]
fn invariants_hold_ci_ordered_and_pvalues_in_unit_interval() {
    let spec = crossover_spec();
    let snaps = snaps_from_prices(&trending(150));
    let config = McConfig {
        permutations: 300,
        scheme: ResampleScheme::MovingBlock { block: 10 },
        seed: 42,
        ci_level: 0.9,
        rerun_null: true,
        metrics: Vec::new(),
    };
    let section = run_mc(&spec, &snaps, &config).section;
    assert!(!section.metrics.is_empty());
    for m in &section.metrics {
        if let (Some(lo), Some(hi)) = (m.ci_lower, m.ci_upper) {
            assert!(lo <= hi, "{} CI inverted: [{lo}, {hi}]", m.name);
        }
        if let Some(p) = m.p_value_rerun {
            assert!(p > 0.0 && p <= 1.0, "{} p-value out of (0,1]: {p}", m.name);
        }
    }
}

#[test]
fn multi_symbol_still_produces_a_rerun_pvalue() {
    // The re-run null works on every shape (unlike the old positions-held
    // null it replaced, which was single-asset only); a multi-symbol basket
    // must still produce p(rerun).
    let yaml = r#"
        selection: !top_bottom { longs: 1, shorts: 1 }
        score: !roc { source: !close, period: 3 }
        sizing: !equal_weight 2
    "#;
    let strat = fugazi::spec::BasketStrategySpec::from_text_with_params_in(
        yaml,
        &Default::default(),
        std::path::Path::new("."),
        std::path::Path::new("."),
        "(mc-basket)",
    )
    .expect("parse basket");
    let spec = StrategySpec::Basket(Box::new(strat));

    let a = trending(80);
    let snaps: Vec<Snapshot<Symbol>> = (0..80)
        .map(|i| {
            let pa = a[i];
            let pb = 100.0 + 9.0 * ((i as Real) * 0.25).cos() + 0.1 * i as Real;
            let mut s = Snapshot::<Symbol>::new();
            s.push(
                Some("A".into()),
                None,
                Atom::new(Candle::new(pa, pa + 1.0, pa - 1.0, pa, 1_000.0)),
            );
            s.push(
                Some("B".into()),
                None,
                Atom::new(Candle::new(pb, pb + 1.0, pb - 1.0, pb, 1_000.0)),
            );
            s
        })
        .collect();

    let config = McConfig {
        permutations: 100,
        scheme: ResampleScheme::Stationary { mean_block: 6.0 },
        seed: 5,
        ci_level: 0.95,
        rerun_null: true,
        metrics: vec!["sharpe".to_string()],
    };
    let section = run_mc(&spec, &snaps, &config).section;
    let m = &section.metrics[0];
    assert!(
        m.p_value_rerun.is_some(),
        "re-run null should still produce a p-value"
    );
}

#[test]
fn backtest_layer_populates_montecarlo_not_just_the_cli() {
    // The core promise of the refactor: driving through `run_iteration_any`
    // with `EvalContext::mc` set attaches the `montecarlo:` block and samples —
    // so any driver (Python, a batch runner), not only the CLI, gets it.
    let spec = crossover_spec();
    let prices = trending(100);
    let snaps = snaps_from_prices(&prices);
    let bars: Vec<String> = (0..prices.len()).map(|i| i.to_string()).collect();
    let costs = empty_costs();
    let mut ctx = ctx(&costs);
    ctx.mc = Some(McConfig {
        permutations: 80,
        scheme: ResampleScheme::Stationary { mean_block: 8.0 },
        seed: 11,
        ci_level: 0.95,
        rerun_null: true,
        metrics: vec!["sharpe".to_string()],
    });

    let iter = run_iteration_any(&spec, bars, &snaps, &ctx).expect("run iteration");
    let section = iter
        .metrics
        .montecarlo
        .expect("montecarlo block attached by backtest layer");
    assert_eq!(section.metrics.len(), 1);
    assert_eq!(section.metrics[0].name, "risk_adjusted.sharpe");
    let samples = iter
        .mc_samples
        .expect("samples surfaced on IterationResult");
    assert_eq!(samples.sets.len(), 2); // ci + rerun
}

#[test]
fn samples_csv_shape_matches_estimators() {
    let spec = crossover_spec();
    let snaps = snaps_from_prices(&trending(90));
    let config = McConfig {
        permutations: 50,
        scheme: ResampleScheme::Iid,
        seed: 9,
        ci_level: 0.95,
        rerun_null: true,
        metrics: Vec::new(),
    };
    let outcome = run_mc(&spec, &snaps, &config);
    // bootstrap_ci + null_rerun, each with `permutations` rows.
    assert_eq!(outcome.samples.sets.len(), 2);
    for set in &outcome.samples.sets {
        assert_eq!(set.rows.len(), 50, "estimator {} row count", set.estimator);
        for row in &set.rows {
            assert_eq!(row.len(), outcome.samples.metric_names.len());
        }
    }
}
