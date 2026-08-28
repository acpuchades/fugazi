//! End-to-end: `fugazi get -x`, the warm-up trim, and its `--keep-unstable`
//! opt-out.
//!
//! `-x/--overlay` and `--keep-unstable` appeared **nowhere** in the suite. What
//! coverage existed was on either side of the driving code: `src/cli/overlay.rs`
//! unit-tests the *parsing* (scope grammar, `active_for`, `stable_bars`), and
//! `tests/overlay_cross_symbol.rs` / `tests/overlays_typed.rs` exercise the
//! *library* overlay layer. `get`'s `apply_overlays` — grouping bars by
//! `(symbol, interval)`, building one instance per group, driving each with the
//! whole-market snapshot, then trimming the warm-up — sat between them untested.
//!
//! `--keep-unstable` matters beyond wiring: it is the named opt-out for one of
//! the crate's safe-by-default rules ("numbers during warm-up are unsettled"),
//! so a default that silently stopped trimming, or an opt-out that silently
//! stopped keeping, is a correctness change with no visible symptom.
//!
//! Every expected value is arithmetic on the literal closes below, so a wrong
//! answer is a wrong number rather than a missing one.

mod common;

use common::cli::{Cmd, unique_path};

/// Six daily bars whose closes are `10, 20, … 60`, so an SMA(3) is exactly the
/// mean of three consecutive multiples of ten: `20, 30, 40, 50` on bars 3..6.
/// Flat bars (`open == high == low == close`) keep every column checkable by eye.
const CLOSES: [i32; 6] = [10, 20, 30, 40, 50, 60];

/// The input CSV in `get`'s own output shape, so a fetched file round-trips.
fn input(symbols: &[&str]) -> String {
    let mut csv = String::from("symbol,freq,time,open,high,low,close,volume\n");
    for sym in symbols {
        // One symbol's closes are the base series; a second gets ten times them,
        // so a scoped overlay landing on the wrong series is a wrong *number*.
        let scale = if *sym == symbols[0] { 1 } else { 10 };
        for (i, close) in CLOSES.iter().enumerate() {
            let c = close * scale;
            let day = i + 1;
            csv.push_str(&format!("{sym},1d,2024-01-0{day},{c},{c},{c},{c},100\n"));
        }
    }
    csv
}

/// Write the input, run `get file:… -x …`, and hand back the output CSV's lines.
fn get(symbols: &[&str], extra: &[&str]) -> Vec<String> {
    let src = unique_path("get_overlay_in.csv");
    std::fs::write(&src, input(symbols)).expect("write input csv");
    let out = unique_path("get_overlay_out.csv");
    let spec = format!("file:{}", src.to_str().expect("utf-8 path"));

    Cmd::new("get")
        .arg(&spec)
        .args(&["-o", out.to_str().expect("utf-8 path"), "-q"])
        .args(extra)
        .ok();

    let text = std::fs::read_to_string(&out).expect("get wrote its output");
    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&out);
    text.lines().map(str::to_string).collect()
}

/// The value of column `col` on every row, `None` for a blank cell.
fn column(lines: &[String], col: &str) -> Vec<Option<String>> {
    let header: Vec<&str> = lines[0].split(',').collect();
    let idx = header
        .iter()
        .position(|h| *h == col)
        .unwrap_or_else(|| panic!("no `{col}` column in header {:?}", lines[0]));
    lines[1..]
        .iter()
        .map(|line| {
            let cell = line.split(',').nth(idx).unwrap_or("");
            (!cell.is_empty()).then(|| cell.to_string())
        })
        .collect()
}

/// The `symbol` cell of every row.
fn symbols_of(lines: &[String]) -> Vec<String> {
    column(lines, "symbol")
        .into_iter()
        .map(|c| c.expect("every row names its symbol"))
        .collect()
}

/// **The column is computed, and the warm-up rows are dropped.**
///
/// An SMA(3) over `10, 20, 30, 40, 50, 60` reads `20, 30, 40, 50` from bar 3 on
/// and nothing before, so the default output is four rows, not six.
#[test]
fn an_overlay_column_is_computed_and_its_warm_up_trimmed() {
    let lines = get(&["BTC"], &["-x", "sma3=!sma { period: 3 }"]);

    assert_eq!(
        lines.len() - 1,
        4,
        "the two pre-warm-up rows must be dropped:\n{}",
        lines.join("\n")
    );
    assert_eq!(
        column(&lines, "sma3"),
        ["20", "30", "40", "50"]
            .map(|v| Some(v.to_string()))
            .to_vec(),
    );
    // The OHLCV columns survive alongside the new one.
    assert_eq!(
        column(&lines, "close"),
        ["30", "40", "50", "60"]
            .map(|v| Some(v.to_string()))
            .to_vec(),
    );
}

/// **`--keep-unstable` keeps them, blank.**
///
/// The opt-out's whole job: the same six bars come back, and the two rows the
/// default dropped are present with an *empty* `sma3` cell rather than a made-up
/// number.
#[test]
fn keep_unstable_emits_the_warm_up_rows_with_blank_overlays() {
    let lines = get(
        &["BTC"],
        &["-x", "sma3=!sma { period: 3 }", "--keep-unstable"],
    );

    assert_eq!(lines.len() - 1, CLOSES.len(), "every bar must be emitted");
    assert_eq!(
        column(&lines, "sma3"),
        vec![
            None,
            None,
            Some("20".into()),
            Some("30".into()),
            Some("40".into()),
            Some("50".into()),
        ],
        "the pre-warm-up cells must be blank, not zero-filled"
    );
}

/// A fetch with no overlay at all is not trimmed — there is nothing to warm up.
///
/// Without this, the assertion above would also pass for a `get` that trimmed
/// two rows off the front of every output unconditionally.
#[test]
fn a_fetch_with_no_overlay_keeps_every_bar() {
    let lines = get(&["BTC"], &[]);
    assert_eq!(lines.len() - 1, CLOSES.len());
    assert!(
        !lines[0].contains("sma"),
        "no overlay was asked for: {}",
        lines[0]
    );
}

/// **A scoped overlay lands on the series it names, and only that one.**
///
/// `BTC:` restricts the column to BTC's `(symbol, interval)` group. ETH's rows
/// still appear — the fetch is not filtered — but its cells stay blank. The
/// second series carries ten times the closes, so a column computed on the wrong
/// group would be a wrong number rather than a missing one.
#[test]
fn a_scoped_overlay_applies_to_the_named_series_only() {
    let lines = get(
        &["BTC", "ETH"],
        &["-x", "BTC:sma3=!sma { period: 3 }", "--keep-unstable"],
    );

    let syms = symbols_of(&lines);
    let sma = column(&lines, "sma3");
    assert_eq!(syms.len(), 2 * CLOSES.len(), "both series are emitted");

    let btc: Vec<Option<String>> = sma
        .iter()
        .zip(&syms)
        .filter(|(_, s)| *s == "BTC")
        .map(|(v, _)| v.clone())
        .collect();
    let eth: Vec<Option<String>> = sma
        .iter()
        .zip(&syms)
        .filter(|(_, s)| *s == "ETH")
        .map(|(v, _)| v.clone())
        .collect();

    assert_eq!(
        btc,
        vec![
            None,
            None,
            Some("20".into()),
            Some("30".into()),
            Some("40".into()),
            Some("50".into()),
        ],
        "BTC is the scoped series"
    );
    assert!(
        eth.iter().all(Option::is_none),
        "ETH is out of scope and must stay blank, got {eth:?}"
    );
}

/// **Each group holds its own indicator state.** Two symbols, one unscoped
/// overlay: the SMA of ETH's `100..600` is `200, 300, 400, 500`, not something
/// contaminated by BTC's interleaved bars.
#[test]
fn an_unscoped_overlay_computes_per_series() {
    let lines = get(&["BTC", "ETH"], &["-x", "sma3=!sma { period: 3 }"]);

    let syms = symbols_of(&lines);
    let sma = column(&lines, "sma3");
    let of = |want: &str| -> Vec<Option<String>> {
        sma.iter()
            .zip(&syms)
            .filter(|(_, s)| *s == want)
            .map(|(v, _)| v.clone())
            .collect()
    };

    assert_eq!(
        of("BTC"),
        ["20", "30", "40", "50"]
            .map(|v| Some(v.to_string()))
            .to_vec()
    );
    assert_eq!(
        of("ETH"),
        ["200", "300", "400", "500"]
            .map(|v| Some(v.to_string()))
            .to_vec()
    );
}

/// `--params` resolves a `!param` inside an overlay expression, exactly as
/// `run --params` does for a strategy document.
#[test]
fn params_reach_an_overlay_expression() {
    let lines = get(
        &["BTC"],
        &["-p", "N=2", "-x", "ma=!sma { period: !param N }"],
    );
    // SMA(2) over 10,20,…,60 is 15, 25, 35, 45, 55 — five rows, one trimmed.
    assert_eq!(
        column(&lines, "ma"),
        ["15", "25", "35", "45", "55"]
            .map(|v| Some(v.to_string()))
            .to_vec(),
        "the period must come from --params, not a default"
    );
}

/// A reserved column name is refused rather than silently shadowing an OHLCV
/// column in the output.
#[test]
fn an_overlay_may_not_take_a_reserved_column_name() {
    let src = unique_path("get_overlay_reserved.csv");
    std::fs::write(&src, input(&["BTC"])).expect("write input csv");
    let out = unique_path("get_overlay_reserved_out.csv");

    let outcome = Cmd::new("get")
        .arg(&format!("file:{}", src.to_str().expect("utf-8 path")))
        .args(&["-o", out.to_str().expect("utf-8 path"), "-q"])
        .args(&["-x", "close=!sma { period: 3 }"])
        .fails();
    assert!(
        outcome.stderr.contains("close"),
        "the diagnostic must name the column: {}",
        outcome.stderr
    );
    let _ = std::fs::remove_file(&src);
}
