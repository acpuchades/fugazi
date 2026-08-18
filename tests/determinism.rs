//! The backtester has to agree with itself: one spec over one dataset must
//! produce one answer, run after run, process after process.
//!
//! That is not free. The cross-sectional layer ranks a `HashMap<Sym, Real>`
//! of scores, and `RandomState` reseeds its iteration order every process —
//! so any rank that truncates (`!top_bottom`, `!quantile`) picks a different
//! basket on every run unless it breaks ties on something stable. It did not,
//! and the cost was silent: a tie-heavy universe returned a different equity
//! curve each time, with no warning, and nothing downstream could tell a real
//! edge from a lucky seed.
//!
//! Ties are ordinary, not exotic — a score saturates, peaks at a bound, or is
//! a constant `!if_else` branch — so this is pinned end to end rather than
//! only in the unit tests beside `ranked_take`. **Re-running the binary is
//! the point**: `RandomState` is seeded once per process, so a loop inside
//! one test process cannot observe the bug at all.

mod common;

use common::cli::{Cmd, scratch_file};

/// Six symbols on staggered sine waves, scored by a two-valued `!if_else` —
/// so the score is 1.0 or 0.0 and several symbols always share whichever
/// value, while `longs: 3` of a 6-symbol universe forces the rank to
/// actually break those ties.
const TIE_BASKET: &str = "\
selection: !top_bottom { longs: 3, shorts: 0 }
score: !if_else
  cond: !gt
    lhs: !close { source: !pick { symbol: !arg SYM } }
    rhs: !sma { source: !close { source: !pick { symbol: !arg SYM } }, period: 10 }
  then: !value 1.0
  otherwise: !value 0.0
sizing: !value 0.3333
universe: !any_of [AAA, BBB, CCC, DDD, EEE, FFF]
";

/// Staggered sine waves: each symbol crosses its own average on a different
/// bar, so the tied *set* churns rather than being the same three symbols for
/// the whole run.
fn tie_series() -> String {
    let mut out = String::from("symbol;time;open;high;low;close;volume\n");
    for d in 0..120 {
        let date = format!("2024-{:02}-{:02}", d / 28 + 1, d % 28 + 1);
        for (i, sym) in ["AAA", "BBB", "CCC", "DDD", "EEE", "FFF"].iter().enumerate() {
            let p = 100.0 + 10.0 * (((d + i * 3) as f64) / 7.0).sin();
            out += &format!(
                "{sym};{date}T00:00:00Z;{p:.2};{:.2};{:.2};{:.2};1000\n",
                p + 1.0,
                p - 1.0,
                p + 0.3,
            );
        }
    }
    out
}

/// Every artefact `run` writes, so a divergence is caught wherever it
/// surfaces — the headline metrics, the equity path, and the fills that
/// produced it.
const ARTEFACTS: [&str; 4] = ["metrics.yml", "returns.csv", "trades.csv", "fills.csv"];

/// Three processes, byte-identical output. Before the rank broke ties on the
/// symbol this produced three different equity curves — the reported spread
/// was 7.1x over five runs.
#[test]
fn a_tie_heavy_basket_runs_identically_every_process() {
    let (_, strategy) = scratch_file("tie_basket.yml", TIE_BASKET);
    let (_, series) = scratch_file("tie_universe.csv", &tie_series());

    // A fresh output dir per run, so run N cannot read run N-1's artefacts.
    let run_once = || {
        let out = Cmd::new("run")
            .arg(&format!("basket:{strategy}"))
            .series(&series)
            .args(&["--crypto", "-f", "1d", "-c", "100000", "--quiet"])
            .costs("none")
            .output_dir("tie_run")
            .ok();
        ARTEFACTS.map(|name| out.read(name))
    };

    let first = run_once();

    // A basket that never traded would pass this vacuously.
    assert!(
        first[2].lines().count() > 2,
        "fixture stopped trading, so it no longer exercises the rank:\n{}",
        first[2]
    );

    for attempt in 2..=3 {
        let again = run_once();
        for (name, (a, b)) in ARTEFACTS.iter().zip(first.iter().zip(again.iter())) {
            assert_eq!(
                a, b,
                "run {attempt} disagreed with run 1 on {name} — the backtest is \
                 not deterministic"
            );
        }
    }
}
