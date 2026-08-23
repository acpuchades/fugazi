//! Cross-validation of `PaperWallet`'s execution arithmetic against
//! [vectorbt](https://github.com/polakowo/vectorbt).
//!
//! The other two cross-checks pin *numbers derived from an equity curve*
//! (`metrics_validation.rs`) and *numbers derived from a price series*
//! (`talib_validation.rs`). Neither has ever seen a fill. This one starts one
//! layer earlier: it replays a fixed order schedule through the wallet and
//! compares the cash, position and equity it produces, bar by bar, against
//! vectorbt's.
//!
//! Both sides consume `tests/data/wallet_bars.csv` — the bars *and* the
//! schedule — so the comparison is valid regardless of how representative the
//! prices are, exactly as in the TA-Lib suite.
//!
//! # What this pins that nothing else does
//!
//! **The fill-timing rule.** fugazi queues a market order at bar N and fills it
//! at bar N+1's open (docs/TRADING.md §2). The generator expresses that by
//! placing vectorbt's order one row later, priced at that row's open. Were the
//! wallet ever to fill at the signal bar's close, every cash and equity figure
//! downstream would shift and this suite would go red. That is lookahead — the
//! most consequential thing a backtester can get wrong and the least likely to
//! announce itself.
//!
//! **The cost pipeline, one leg at a time.** Five configurations run over the
//! same schedule (`zero`, `commission`, `spread`, `slippage`, `full`) so a
//! mismatch names the leg that caused it rather than "costs".
//!
//! # What it deliberately does not pin
//!
//! Stops, take-profits, resting limits, the intrabar ordering between them
//! (§3), fractional `value_frac` shrink-to-fit, and portfolio netting. vectorbt
//! shares none of those semantics, so there is no independent opinion to check
//! against — they stay covered by the unit tests in `src/wallet/paper.rs` and
//! by `tests/portfolio.rs`. The schedule here is market orders at explicit unit
//! targets, which is the subset both engines agree on.
//!
//! **Nor the composition of the two price legs.** `full` runs commission and
//! slippage but *not* spread, because fugazi composes spread and slippage
//! multiplicatively and vectorbt has a single adverse-price knob: folding both
//! into it yields `a + b + ab` on a buy and `a + b − ab` on a sell, so one
//! fraction can match one side or the other, never both. Spread is therefore
//! checked alone, where the mapping is exact. `tools/gen_wallet_fixtures.py`
//! carries the derivation; the composition itself stays with the unit tests in
//! `src/costs/mod.rs`.
//!
//! Constants (`INITIAL_CASH`, `COMMISSION_RATE`, `SLIPPAGE_BPS`, `SPREAD_BPS`)
//! must match `tools/gen_wallet_fixtures.py`.

mod common;

use fugazi::costs::{
    FixedBpsSlippage, FixedBpsSpread, NoCommission, NoSlippage, NoSpread, PercentageCommission,
    TradingCosts,
};
use fugazi::prelude::*;
use fugazi::wallet::{PaperWallet, Units};

use common::fixtures::{Csv, skip};

/// Must match `tools/gen_wallet_fixtures.py`.
const INITIAL_CASH: Real = 10_000.0;
const COMMISSION_RATE: Real = 0.001;
const SLIPPAGE_BPS: Real = 5.0;
const SPREAD_BPS: Real = 8.0;

const SYMBOL: &str = "TEST";

/// Cash and position are exact float arithmetic on both sides — the same
/// multiplications in the same order — so the only slack is the last ULP or two
/// of accumulated rounding across 60 bars.
const TOL: Real = 1e-9;

/// The five cost configurations the generator writes, by fixture prefix.
const CONFIGS: [&str; 5] = ["zero", "commission", "spread", "slippage", "full"];

fn costs_for(config: &str) -> TradingCosts {
    let mut costs = TradingCosts {
        carry: Box::new(fugazi::costs::NoCarry),
        commission: Box::new(NoCommission),
        spread: Box::new(NoSpread),
        slippage: Box::new(NoSlippage),
    };
    match config {
        "zero" => {}
        "commission" => costs.commission = Box::new(PercentageCommission::new(COMMISSION_RATE)),
        "spread" => costs.spread = Box::new(FixedBpsSpread::new(SPREAD_BPS)),
        "slippage" => costs.slippage = Box::new(FixedBpsSlippage::new(SLIPPAGE_BPS)),
        // No spread: see the module docstring — the two price legs cannot both
        // be expressed through vectorbt's single knob.
        "full" => {
            costs.commission = Box::new(PercentageCommission::new(COMMISSION_RATE));
            costs.slippage = Box::new(FixedBpsSlippage::new(SLIPPAGE_BPS));
        }
        other => panic!("unknown cost configuration `{other}`"),
    }
    costs
}

/// One bar of the committed input: the candle, plus the position the schedule
/// asks for at its close (`None` = no submission).
struct Bar {
    candle: Candle,
    target: Option<Real>,
}

fn load_input() -> Vec<Bar> {
    let csv = Csv::require("wallet_bars.csv");
    let (open, high, low, close, volume) = (
        csv.floats("open"),
        csv.floats("high"),
        csv.floats("low"),
        csv.floats("close"),
        csv.floats("volume"),
    );
    let target = csv.optional_floats("target");
    (0..csv.len())
        .map(|i| Bar {
            candle: Candle::new(open[i], high[i], low[i], close[i], volume[i]),
            target: target[i],
        })
        .collect()
}

/// Drive a `PaperWallet` over the schedule, mirroring `backtest::run`'s per-bar
/// order: price the wallet (which is where queued fills are born), then submit,
/// then record. Submission cannot move equity — it queues — so the recording
/// point is unambiguous.
///
/// Returns `(cash, position, equity)` per bar.
fn drive(bars: &[Bar], costs: TradingCosts) -> (Vec<Real>, Vec<Real>, Vec<Real>) {
    let mut wallet: PaperWallet<&'static str> = PaperWallet::with_costs(INITIAL_CASH, costs);
    let (mut cash, mut position, mut equity) = (Vec::new(), Vec::new(), Vec::new());

    for bar in bars {
        wallet.update(SYMBOL, bar.candle);
        if let Some(target) = bar.target {
            wallet
                .set_position(Units {
                    symbol: SYMBOL,
                    amount: target,
                })
                .expect("the committed schedule is affordable at every bar");
        }
        cash.push(wallet.funds().0);
        position.push(wallet.position(&SYMBOL).amount);
        equity.push(wallet.equity().0);
    }

    // A rejected fill would leave the wallet in a state vectorbt never entered,
    // and the comparison below would then be measuring the divergence rather
    // than the arithmetic. `raise_reject=True` holds the same line on the
    // generator's side.
    assert!(
        wallet.rejections().is_empty(),
        "wallet rejected {} order(s); the schedule must stay affordable: {:?}",
        wallet.rejections().len(),
        wallet.rejections()
    );
    (cash, position, equity)
}

#[test]
fn wallet_matches_vectorbt() {
    let expected = match Csv::load("wallet_expected.csv") {
        Some(csv) => csv,
        None => {
            skip(
                "wallet_validation",
                "tests/data/wallet_expected.csv is not present",
                "  pixi run gen-wallet\n  cargo test --test wallet_validation",
            );
            return;
        }
    };

    // Staleness, not just absence: a fixture generated before a configuration
    // was added is missing its columns, and comparing the ones it does have
    // would quietly pass while checking less than it claims.
    let wanted: Vec<String> = CONFIGS
        .iter()
        .flat_map(|c| ["cash", "position", "equity"].map(|f| format!("{c}.{f}")))
        .collect();
    let refs: Vec<&str> = wanted.iter().map(String::as_str).collect();
    if let Some(missing) = expected.missing(&refs) {
        skip(
            "wallet_validation",
            &format!("tests/data/wallet_expected.csv has no `{missing}` column"),
            "  pixi run gen-wallet\n  cargo test --test wallet_validation",
        );
        return;
    }

    let bars = load_input();
    assert!(
        expected.len() == bars.len(),
        "fixture has {} rows, input has {} bars — regenerate wallet_expected.csv",
        expected.len(),
        bars.len()
    );

    let mut mismatches: Vec<String> = Vec::new();
    let mut compared = 0usize;

    for config in CONFIGS {
        let (cash, position, equity) = drive(&bars, costs_for(config));
        for (field, got) in [
            ("cash", &cash),
            ("position", &position),
            ("equity", &equity),
        ] {
            let want = expected.floats(&format!("{config}.{field}"));
            for (bar, (&g, &w)) in got.iter().zip(want.iter()).enumerate() {
                compared += 1;
                let tol = TOL.max(w.abs() * 1e-9);
                if (g - w).abs() > tol {
                    mismatches.push(format!(
                        "{config}.{field}[{bar}]: got {g}, expected {w}, \
                         diff {} (tol {tol})",
                        (g - w).abs()
                    ));
                }
            }
        }
    }

    // A present-but-empty fixture would make the loop above a no-op and this
    // suite pass vacuously — the same silent-rot failure the skip policy exists
    // to prevent, one step further in.
    assert!(
        compared >= bars.len() * CONFIGS.len() * 3,
        "compared only {compared} cells — regenerate wallet_expected.csv"
    );

    assert!(
        mismatches.is_empty(),
        "vectorbt-reference divergence ({} of {compared} cells):\n  {}",
        mismatches.len(),
        // A cost-model change moves every bar after the first fill, so printing
        // all of them buries the one that matters: the earliest.
        mismatches
            .iter()
            .take(15)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n  ")
    );
}

/// The schedule must actually reach the states it claims to, or the comparison
/// above is 720 cells of agreeing about nothing. Held here rather than in the
/// generator so a hand-edit to the committed CSV cannot quietly defeat it.
#[test]
fn schedule_exercises_long_short_and_reversal() {
    let bars = load_input();
    let (_, position, _) = drive(&bars, costs_for("zero"));

    assert!(
        position.iter().any(|&p| p > 0.0),
        "schedule never opens a long"
    );
    assert!(
        position.iter().any(|&p| p < 0.0),
        "schedule never opens a short"
    );
    assert!(
        position.windows(2).any(|w| w[0] < 0.0 && w[1] > 0.0),
        "schedule never reverses a short straight through zero into a long — \
         the path where `fill_at` must both close and open in one delta"
    );
    assert!(
        position.last().is_some_and(|&p| p == 0.0),
        "schedule must end flat, so the closing equity is pure cash"
    );
}
