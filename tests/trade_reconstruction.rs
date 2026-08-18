//! Regression cover for trade reconstruction on **multi-symbol** blotters.
//!
//! Through 0.63.1 `metrics::reconstruct_trades` walked the whole fill blotter
//! with a single signed position and never read `order.symbol`. It was written
//! for the single-asset case, but `spec::metrics::from_report` hands it
//! `report.fills` — the full multi-symbol blotter — for every document shape.
//! An opposite-side fill in a *different* instrument therefore closed the open
//! leg, and P&L subtracted one asset's price from another's: on the two-asset
//! pairs run below it reported three trades instead of two, booking a −4500
//! loss that never happened on a 10 000 account.
//!
//! The unit-level cover lives in `metrics::tests` (interleaved blotters, the
//! per-symbol replay invariant). This file is the end-to-end half: it drives
//! the real binary so the assertion covers the whole path a user sees —
//! runner → `RunReport` → `from_report` → `trades.csv` — for each multi-symbol
//! shape, since all four route through the same call.
//!
//! **The invariant, and why it is a price band.** The two symbols quote in
//! disjoint decades: `AAA` near 100, `BBB` near 10. Any trade that pairs one
//! symbol's entry with another's exit lands a sub-10 price beside a three-digit
//! one, so "both prices sit in the same band" catches the bug without pinning a
//! trade count per shape — which would otherwise couple this file to each
//! strategy's signal timing rather than to the reconstruction itself.
//!
//! Do **not** relax any of this to `Σpnl == equity change`. The fabricated legs
//! telescope: consecutive bogus trades share prices and cancel, so the original
//! bug reconciled exactly at the total while every individual trade was wrong.

mod common;

use common::cli::{Cmd, scratch_file};

/// Where the two price decades are split. `AAA` quotes above it, `BBB` below.
const BAND: f64 = 50.0;

/// 60 daily bars of two anti-correlated symbols in disjoint price decades.
///
/// `AAA` oscillates around 100 and `BBB` around 10, in antiphase, so a
/// cross-sectional ranking flips repeatedly and the basket/multi shapes below
/// actually turn over instead of opening once and holding.
fn two_decade_series() -> String {
    let mut csv = String::from("symbol,freq,time,open,high,low,close,volume\n");
    for i in 0..60 {
        let phase = f64::from(i) / 6.0;
        let aaa = 100.0 + 10.0 * phase.sin();
        let bbb = 10.0 - 1.0 * phase.sin();
        // 2024-01-01 + i days, as a plain UTC timestamp.
        let day = 1 + i;
        for (sym, px) in [("AAA", aaa), ("BBB", bbb)] {
            csv.push_str(&format!(
                "{sym},1d,2024-{:02}-{:02}T00:00:00Z,{px},{px},{px},{px},1000000\n",
                1 + day / 31,
                1 + day % 31,
            ));
        }
    }
    csv
}

#[derive(Debug)]
struct Row {
    side: String,
    entry_price: f64,
    exit_price: f64,
    pnl: f64,
}

fn parse_trades(csv: &str) -> Vec<Row> {
    let mut lines = csv.lines();
    let header = lines.next().unwrap_or_default();
    assert!(
        header.starts_with("entry_time,exit_time,side,units,entry_price,exit_price,pnl"),
        "unexpected trades.csv header: {header}"
    );
    lines
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let f: Vec<&str> = l.split(',').collect();
            Row {
                side: f[2].to_string(),
                entry_price: f[4].parse().expect("entry_price"),
                exit_price: f[5].parse().expect("exit_price"),
                pnl: f[6].parse().expect("pnl"),
            }
        })
        .collect()
}

/// The shape-agnostic assertion: no trade may straddle the price band, because
/// straddling it means the leg was closed by a fill in the other instrument.
#[track_caller]
fn assert_no_cross_symbol_legs(trades: &[Row], shape: &str) {
    assert!(
        !trades.is_empty(),
        "{shape}: no trades at all — the document stopped trading, so this \
         proves nothing; fix the fixture before trusting a green run"
    );
    for t in trades {
        let entry_high = t.entry_price > BAND;
        let exit_high = t.exit_price > BAND;
        assert_eq!(
            entry_high, exit_high,
            "{shape}: trade pairs prices from two different instruments \
             (entry {:.4}, exit {:.4}, pnl {:.4}) — reconstruct_trades closed \
             one symbol's leg with another symbol's fill\nall trades: {trades:#?}",
            t.entry_price, t.exit_price, t.pnl,
        );
    }
}

/// Run `shape:@spec` over the two-decade series and hand back `trades.csv`.
fn run_shape(shape: &str, out_name: &str, spec: &str, extra: &[&str]) -> Vec<Row> {
    let (_csv_path, csv_arg) = scratch_file(&format!("{out_name}.csv"), &two_decade_series());
    let (_spec_path, spec_arg) = scratch_file(&format!("{out_name}.yml"), spec);
    let out = Cmd::new("run")
        .arg(&format!("{shape}:{spec_arg}"))
        .series(&csv_arg)
        .costs("none")
        .args(&["--crypto", "-f", "1d"])
        .args(extra)
        .output_dir(out_name)
        .ok();
    parse_trades(&out.read("trades.csv"))
}

/// The reported repro, verbatim in spirit: `AAA` held long from 100 to 110 and
/// `BBB` held short from 10 to 9, both flattened on the last bar. Two clean
/// round trips, both winners — where 0.63.1 reported three trades at 66.7% win
/// rate, the middle one real and the outer two fabricated.
///
/// This is the one shape whose trade count is pinned, because the signals are
/// constants: `enter` is always true and `exit` never fires, so the only fills
/// are the two entries and the two `--flatten` exits.
#[test]
fn pairs_round_trips_do_not_close_each_other() {
    const CSV: &str = "\
symbol,freq,time,open,high,low,close,volume
AAA,1d,2024-01-01T00:00:00Z,100,100,100,100,1000000
BBB,1d,2024-01-01T00:00:00Z,10,10,10,10,1000000
AAA,1d,2024-01-02T00:00:00Z,100,100,100,100,1000000
BBB,1d,2024-01-02T00:00:00Z,10,10,10,10,1000000
AAA,1d,2024-01-03T00:00:00Z,100,100,100,100,1000000
BBB,1d,2024-01-03T00:00:00Z,10,10,10,10,1000000
AAA,1d,2024-01-04T00:00:00Z,110,110,110,110,1000000
BBB,1d,2024-01-04T00:00:00Z,9,9,9,9,1000000
AAA,1d,2024-01-05T00:00:00Z,110,110,110,110,1000000
BBB,1d,2024-01-05T00:00:00Z,9,9,9,9,1000000
AAA,1d,2024-01-06T00:00:00Z,110,110,110,110,1000000
BBB,1d,2024-01-06T00:00:00Z,9,9,9,9,1000000
";
    const SPEC: &str = "\
left: AAA
right: BBB
enter: !above { source: !value 1.0, level: 0.0 }
exit: !never
sizing: !value 1.0
";
    let (_csv, csv_arg) = scratch_file("trade_recon_pairs.csv", CSV);
    let (_spec, spec_arg) = scratch_file("trade_recon_pairs.yml", SPEC);
    let out = Cmd::new("run")
        .arg(&format!("pairs:{spec_arg}"))
        .series(&csv_arg)
        .costs("none")
        .args(&["--crypto", "-f", "1d", "--flatten"])
        .output_dir("trade_recon_pairs")
        .ok();

    let trades = parse_trades(&out.read("trades.csv"));
    assert_no_cross_symbol_legs(&trades, "pairs");
    assert_eq!(
        trades.len(),
        2,
        "one round trip per leg; 0.63.1 reported 3\n{trades:#?}"
    );

    // Both legs are winners: AAA long 100 → 110 and BBB short 10 → 9.
    assert!(
        trades.iter().all(|t| t.pnl > 0.0),
        "both legs profit; the −4500 leg is the fabricated one\n{trades:#?}"
    );

    let long = trades.iter().find(|t| t.side == "long").expect("a long leg");
    let short = trades
        .iter()
        .find(|t| t.side == "short")
        .expect("a short leg");
    assert!((long.entry_price - 100.0).abs() < 1e-9);
    assert!((long.exit_price - 110.0).abs() < 1e-9);
    assert!((long.pnl - 500.0).abs() < 1e-6);
    assert!((short.entry_price - 10.0).abs() < 1e-9);
    assert!((short.exit_price - 9.0).abs() < 1e-9);
    assert!((short.pnl - 500.0).abs() < 1e-6);

    // The metrics document is reduced from the same trades, so it moves with
    // them: 0.63.1 reported `total: 3` / `win_rate_pct: 66.66…` here.
    let metrics = out.read("metrics.yml");
    for want in ["total: 2", "wins: 2", "losses: 0", "win_rate_pct: 100.0"] {
        assert!(
            metrics.contains(want),
            "metrics.yml missing `{want}` — it disagrees with trades.csv:\n{metrics}"
        );
    }
    assert!(
        metrics.contains("long_trades: 1") && metrics.contains("short_trades: 1"),
        "one leg per side:\n{metrics}"
    );
}

/// A cross-sectional basket: always one long and one short, rebalanced as the
/// ranking flips. Its blotter interleaves the two symbols densely — the case
/// the single-position walk mangled hardest.
#[test]
fn basket_legs_do_not_close_each_other() {
    const SPEC: &str = "\
selection: !top_bottom { longs: 1, shorts: 1 }
score: !roc
  source: !close { source: !pick { symbol: !arg SYM } }
  period: 5
sizing: !equal_weight 2
";
    let trades = run_shape("basket", "trade_recon_basket", SPEC, &["--flatten"]);
    assert_no_cross_symbol_legs(&trades, "basket");
}

/// The per-symbol long/short shape: each symbol runs its own entry/exit chain,
/// so both trade independently and their fills interleave in one blotter.
#[test]
fn multi_asset_legs_do_not_close_each_other() {
    const SPEC: &str = "\
long:
  enter: !crosses_above
    lhs: !close { source: !pick { symbol: !arg SYM } }
    rhs: !sma { source: !close { source: !pick { symbol: !arg SYM } }, period: 5 }
  exit: !crosses_below
    lhs: !close { source: !pick { symbol: !arg SYM } }
    rhs: !sma { source: !close { source: !pick { symbol: !arg SYM } }, period: 5 }
sizing: !equal_weight 2
";
    let trades = run_shape("multi", "trade_recon_multi", SPEC, &["--flatten"]);
    assert_no_cross_symbol_legs(&trades, "multi");
}

/// A portfolio of two single-asset children nets onto one account, so the
/// blotter it reduces from carries both symbols too.
#[test]
fn portfolio_children_do_not_close_each_other() {
    const SPEC: &str = "\
children:
  - name: a
    strategy:
      symbol: AAA
      long:
        enter: !crosses_above
          lhs: !close
          rhs: !sma { source: !close, period: 5 }
        exit: !crosses_below
          lhs: !close
          rhs: !sma { source: !close, period: 5 }
  - name: b
    strategy:
      symbol: BBB
      long:
        enter: !crosses_above
          lhs: !close
          rhs: !sma { source: !close, period: 5 }
        exit: !crosses_below
          lhs: !close
          rhs: !sma { source: !close, period: 5 }
weights: !equal_weight
";
    let trades = run_shape("portfolio", "trade_recon_portfolio", SPEC, &["--flatten"]);
    assert_no_cross_symbol_legs(&trades, "portfolio");
}
