//! End-to-end tests of the bar-cadence census — the diagnostic that decides
//! which frequency a `run` or an `optimize` is actually targeting, and says so
//! when the input can't answer.
//!
//! The failure it closes was silent by construction. `--series` used to join on
//! `(symbol, time)`, and `fugazi get binance:BTCUSDT[1d,1h]` writes both
//! cadences into one file with RFC 3339 stamps — so the daily bar and the
//! midnight hourly bar shared a `time`, merged into one row, and one set of
//! OHLCV survived. The other 23 hourly bars stayed alongside, cadence detection
//! read a ~1h median off the result, and the bar count, the date range and the
//! symbol list all still looked right over a series that was neither of the two
//! the user fetched.
//!
//! So the frame is censused once at load, before anything reads it. Ambiguity
//! is refused; disagreement is reported. These pin the wiring — that each
//! finding reaches the user through the real binary, that `-f/--frequency`
//! disambiguates, and that a single-cadence input is untouched. The rules
//! themselves are unit-tested in `src/cli/cadence.rs`, and the loader's keying
//! in `src/cli/data.rs`.

mod common;

use common::cli::{Cmd, scratch_file, unique_path};

const HEADER: &str = "symbol,freq,time,open,high,low,close,volume\n";

/// A day of `1d` bars for `symbol`, closing at `close`.
fn daily(symbol: &str, day: u32, close: u32) -> String {
    format!("{symbol},1d,2024-01-{day:02}T00:00:00Z,{close},{close},{close},{close},100\n")
}

/// One `1h` bar. `label` is the `freq` cell, so a caller can write a series
/// that is stamped hourly but labelled something else.
fn hourly(symbol: &str, label: &str, day: u32, hour: u32, close: u32) -> String {
    format!(
        "{symbol},{label},2024-01-{day:02}T{hour:02}:00:00Z,{close},{close},{close},{close},100\n"
    )
}

/// `n` consecutive daily bars, closes walking upward so an SMA crossover has
/// something to chew on.
fn daily_series(symbol: &str, n: u32) -> String {
    (1..=n).map(|d| daily(symbol, d, 100 + d)).collect()
}

/// `n` consecutive hourly bars over the first days of January.
fn hourly_series(symbol: &str, label: &str, n: u32) -> String {
    (0..n)
        .map(|i| hourly(symbol, label, 1 + i / 24, i % 24, 100 + i))
        .collect()
}

/// A `--series` argument for a scratch CSV holding `body`.
fn series_of(name: &str, body: &str) -> String {
    let (_, arg) = scratch_file(name, &format!("{HEADER}{body}"));
    arg
}

/// The single-asset SMA-crossover example, retargeted at `symbol`.
fn single_strategy(symbol: &str) -> String {
    let (_, arg) = scratch_file(
        "cadence_single.yml",
        &format!(
            "root: {symbol}\n\
             long:\n  \
               enter: !crosses_above {{ lhs: !sma {{ source: close, period: 2 }}, \
                                        rhs: !sma {{ source: close, period: 4 }} }}\n",
        ),
    );
    arg
}

/// A cross-sectional basket over whatever the frame holds — the shape whose
/// universe is discovered from the stream, and therefore the one a mixed
/// universe mis-annualizes.
fn basket_strategy() -> String {
    let (_, arg) = scratch_file(
        "cadence_basket.yml",
        "selection: !top_bottom { longs: 1, shorts: 0 }\n\
         score: !roc { source: !close { source: !pick { symbol: !slot SYM } }, period: 2 }\n\
         sizing: !equal_weight 1\n",
    );
    format!("basket:{arg}")
}

// --------------------------------------------------------------- ambiguity

/// Two cadences under one symbol, and nothing choosing between them. The old
/// behaviour was a successful run over the collided rows.
#[test]
fn a_symbol_carrying_two_cadences_stops_the_run() {
    let body = daily_series("BTC", 12) + &hourly_series("BTC", "1h", 48);
    let out = Cmd::new("run")
        .arg(&single_strategy("BTC"))
        .series(&series_of("cadence_ambiguous.csv", &body))
        .output_dir("cadence_ambiguous")
        .fails();

    assert!(
        out.stderr.contains("carries 2 cadences"),
        "no ambiguity error:\n{}",
        out.stderr
    );
    // Both cadences named with their weight — which stray series leaked in is
    // the thing the user has to know to fix it.
    assert!(
        out.stderr.contains("1d (12 bars), 1h (48 bars)"),
        "cadences not itemised:\n{}",
        out.stderr
    );
    // And the remedy, spelled the way it would be typed.
    assert!(
        out.stderr.contains("-f/--frequency BTC:<CODE>"),
        "no remedy offered:\n{}",
        out.stderr
    );
    // Refused before anything was written.
    assert!(
        !out.wrote("metrics.yml"),
        "the run produced artefacts anyway"
    );
}

/// `-f SYM:CODE` is the disambiguator, and it really selects: the run's bar
/// count is the chosen cadence's, not the file's.
#[test]
fn a_scoped_frequency_selects_one_of_two_cadences() {
    let body = daily_series("BTC", 12) + &hourly_series("BTC", "1h", 48);
    let series = series_of("cadence_select.csv", &body);

    let hourly_run = Cmd::new("run")
        .arg(&single_strategy("BTC"))
        .series(&series)
        .args(&["-f", "BTC:1h"])
        .output_dir("cadence_select_1h")
        .ok();
    assert_eq!(hourly_run.rows("returns.csv").len(), 48);

    let daily_run = Cmd::new("run")
        .arg(&single_strategy("BTC"))
        .series(&series)
        .args(&["-f", "BTC:1d"])
        .output_dir("cadence_select_1d")
        .ok();
    assert_eq!(daily_run.rows("returns.csv").len(), 12);
}

/// Every ambiguous symbol is reported in one pass — fixing a three-symbol mess
/// should not take three runs to discover.
#[test]
fn every_ambiguous_symbol_is_named_at_once() {
    let body = daily_series("BTC", 12)
        + &hourly_series("BTC", "1h", 24)
        + &daily_series("ETH", 12)
        + &hourly_series("ETH", "4h", 24);
    let out = Cmd::new("run")
        .arg(&basket_strategy())
        .series(&series_of("cadence_two_bad.csv", &body))
        .output_dir("cadence_two_bad")
        .fails();

    assert!(
        out.stderr.contains("`BTC` carries 2 cadences"),
        "{}",
        out.stderr
    );
    assert!(
        out.stderr.contains("`ETH` carries 2 cadences"),
        "{}",
        out.stderr
    );
}

/// `-f` naming a cadence the input does not carry is a typo, not a request to
/// annualize at something the bars never ran at.
#[test]
fn a_frequency_the_input_lacks_is_rejected() {
    let body = daily_series("BTC", 12) + &hourly_series("BTC", "1h", 48);
    let out = Cmd::new("run")
        .arg(&single_strategy("BTC"))
        .series(&series_of("cadence_absent.csv", &body))
        .args(&["-f", "BTC:5m"])
        .output_dir("cadence_absent")
        .fails();

    assert!(
        out.stderr.contains("asks for `5m` on `BTC`"),
        "no absent-cadence error:\n{}",
        out.stderr
    );
    assert!(
        out.stderr.contains("these cadences for it: 1d, 1h"),
        "available cadences not listed:\n{}",
        out.stderr
    );
}

/// The single-cadence form of the same mistake: `-f BTC:1h` over a file whose
/// BTC rows all say `1d`. This used to annualize at 1h without comment.
#[test]
fn a_frequency_contradicting_the_declared_one_is_rejected() {
    let out = Cmd::new("run")
        .arg(&single_strategy("BTC"))
        .series(&series_of(
            "cadence_contradict.csv",
            &daily_series("BTC", 12),
        ))
        .args(&["-f", "BTC:1h"])
        .output_dir("cadence_contradict")
        .fails();

    assert!(
        out.stderr.contains("only one cadence for it: 1d"),
        "no contradiction error:\n{}",
        out.stderr
    );
}

/// Untagged rows beside two labelled cadences can be attached to neither, so
/// they are refused rather than silently dropped — and the error's own remedy
/// is checked to actually work.
#[test]
fn untagged_rows_beside_two_cadences_are_refused_and_the_remedy_works() {
    let prices = series_of(
        "cadence_untagged_px.csv",
        &(daily_series("BTC", 12) + &hourly_series("BTC", "1h", 24)),
    );
    let (overlay_path, overlay_arg) = scratch_file(
        "cadence_untagged_ov.csv",
        &(String::from("symbol,time,sentiment\n")
            + &(1..=12)
                .map(|d| format!("BTC,2024-01-{d:02}T00:00:00Z,0.5\n"))
                .collect::<String>()),
    );

    let out = Cmd::new("run")
        .arg(&single_strategy("BTC"))
        .series(&prices)
        .series(&overlay_arg)
        .args(&["-f", "BTC:1d"])
        .output_dir("cadence_untagged")
        .fails();
    assert!(
        out.stderr.contains("row(s) with no `freq` label"),
        "no untagged-rows error:\n{}",
        out.stderr
    );
    assert!(
        out.stderr.contains("freq=<CODE>"),
        "no remedy offered:\n{}",
        out.stderr
    );

    // The remedy the message advertises, typed out.
    let labelled = format!(
        "freq=1d,@{}",
        overlay_path.to_str().expect("utf-8 scratch path")
    );
    Cmd::new("run")
        .arg(&single_strategy("BTC"))
        .series(&prices)
        .series(&labelled)
        .args(&["-f", "BTC:1d"])
        .output_dir("cadence_untagged_fixed")
        .ok();
}

// -------------------------------------------------------------- warnings

/// A universe whose symbols run at different cadences is measurable but not
/// annualizable by one factor. It warns and continues.
#[test]
fn a_mixed_cadence_universe_warns_and_still_runs() {
    let body = daily_series("BTC", 12) + &daily_series("SOL", 12) + &hourly_series("ETH", "1h", 24);
    let out = Cmd::new("run")
        .arg(&basket_strategy())
        .series(&series_of("cadence_mixed.csv", &body))
        .output_dir("cadence_mixed")
        .ok();

    assert!(
        out.stderr.contains("runs at 2 different cadences"),
        "no mixed-cadence warning:\n{}",
        out.stderr
    );
    // Every cadence named with who runs at it, ascending by duration.
    assert!(
        out.stderr.contains("1h: ETH") && out.stderr.contains("1d: BTC, SOL"),
        "cadence groups not itemised:\n{}",
        out.stderr
    );
    assert!(out.wrote("metrics.yml"), "the run should still complete");
}

/// A label that disagrees with the timestamp spacing. The label is what
/// freq-scoped `--costs` match on; the spacing is what the run is made of.
#[test]
fn a_mislabelled_series_warns() {
    let out = Cmd::new("run")
        .arg(&single_strategy("BTC"))
        // Stamped every hour, labelled `1d`.
        .series(&series_of(
            "cadence_lying.csv",
            &hourly_series("BTC", "1d", 40),
        ))
        .output_dir("cadence_lying")
        .ok();

    assert!(
        out.stderr.contains("labelled `1d` by the `freq` column"),
        "no mislabel warning:\n{}",
        out.stderr
    );
    assert!(
        out.stderr.contains("spaced like `1h`"),
        "detected cadence not reported:\n{}",
        out.stderr
    );
}

/// `--quiet` governs a command's success summary, not a finding about its
/// data — the same bargain `overlap`'s fragmentation warning strikes.
#[test]
fn quiet_does_not_suppress_a_cadence_warning() {
    let body = daily_series("BTC", 12) + &hourly_series("ETH", "1h", 24);
    let out = Cmd::new("run")
        .arg(&basket_strategy())
        .series(&series_of("cadence_quiet.csv", &body))
        .args(&["--quiet"])
        .output_dir("cadence_quiet")
        .ok();

    assert!(
        out.stdout.trim().is_empty(),
        "--quiet still printed:\n{}",
        out.stdout
    );
    assert!(
        out.stderr.contains("runs at 2 different cadences"),
        "--quiet swallowed the warning:\n{}",
        out.stderr
    );
}

// ------------------------------------------------------------- precedence

/// The `freq` column now outranks gap detection when setting the annualization
/// calendar — a provider that told us the cadence beats arithmetic on the bars
/// it sent. With `--crypto`, hourly bars are 8760/year against daily's 365.
#[test]
fn the_freq_column_drives_annualization() {
    let out = Cmd::new("run")
        .arg(&single_strategy("BTC"))
        .series(&series_of(
            "cadence_annual.csv",
            &hourly_series("BTC", "1h", 40),
        ))
        .args(&["--crypto"])
        .output_dir("cadence_annual")
        .ok();
    assert!(
        out.read("metrics.yml").contains("bars_per_year: 8760"),
        "hourly bars did not annualize hourly:\n{}",
        out.read("metrics.yml")
    );

    let daily = Cmd::new("run")
        .arg(&single_strategy("BTC"))
        .series(&series_of("cadence_annual_d.csv", &daily_series("BTC", 40)))
        .args(&["--crypto"])
        .output_dir("cadence_annual_d")
        .ok();
    assert!(
        daily.read("metrics.yml").contains("bars_per_year: 365"),
        "daily bars did not annualize daily:\n{}",
        daily.read("metrics.yml")
    );
}

/// …and an explicit `-f` still outranks the column, as long as it doesn't
/// contradict it. An unlabelled input is the case where the flag is the only
/// evidence there is.
#[test]
fn an_explicit_frequency_still_wins_over_detection() {
    let body: String = (1..=40)
        .map(|i| {
            format!(
                "BTC,,2024-01-{:02}T{:02}:00:00Z,100,100,100,100,1\n",
                1 + i / 24,
                i % 24
            )
        })
        .collect();
    let out = Cmd::new("run")
        .arg(&single_strategy("BTC"))
        .series(&series_of("cadence_flag_wins.csv", &body))
        .args(&["--crypto", "-f", "1d"])
        .output_dir("cadence_flag_wins")
        .ok();
    assert!(
        out.read("metrics.yml").contains("bars_per_year: 365"),
        "the flag did not win:\n{}",
        out.read("metrics.yml")
    );
    // …and the flag disagreeing with the spacing is still worth saying.
    assert!(
        out.stderr.contains("`-f/--frequency`") && out.stderr.contains("spaced like `1h`"),
        "no mislabel warning against the flag:\n{}",
        out.stderr
    );
}

// -------------------------------------------------------------- optimize

/// `optimize` loads its frame through the same path, so a sweep cannot be
/// launched over an ambiguous input either.
#[test]
fn optimize_refuses_an_ambiguous_frame_before_the_sweep() {
    let (_, strategy) = scratch_file(
        "cadence_opt.yml",
        "root: BTC\n\
         long:\n  \
           enter: !crosses_above { lhs: !sma { source: close, period: !param FAST }, \
                                    rhs: !sma { source: close, period: 8 } }\n",
    );
    let body = daily_series("BTC", 12) + &hourly_series("BTC", "1h", 48);
    let out = Cmd::new("optimize")
        .arg(&strategy)
        .series(&series_of("cadence_opt.csv", &body))
        .args(&["--grid", "FAST=[2,3]"])
        .args(&[
            "-o",
            unique_path("cadence_opt.csv").to_str().expect("utf-8"),
        ])
        .fails();

    assert!(
        out.stderr.contains("carries 2 cadences"),
        "optimize did not census its frame:\n{}",
        out.stderr
    );
}

/// The disambiguated sweep runs, over the cadence that was picked.
#[test]
fn optimize_accepts_a_disambiguated_frame() {
    let (_, strategy) = scratch_file(
        "cadence_opt_ok.yml",
        "root: BTC\n\
         long:\n  \
           enter: !crosses_above { lhs: !sma { source: close, period: !param FAST }, \
                                    rhs: !sma { source: close, period: 8 } }\n",
    );
    let body = daily_series("BTC", 12) + &hourly_series("BTC", "1h", 48);
    let results = unique_path("cadence_opt_ok_results.csv");
    Cmd::new("optimize")
        .arg(&strategy)
        .series(&series_of("cadence_opt_ok.csv", &body))
        .args(&["--grid", "FAST=[2,3]"])
        .args(&["-f", "BTC:1h"])
        .args(&["-o", results.to_str().expect("utf-8")])
        .ok();
    let written = std::fs::read_to_string(&results).expect("optimize wrote its results");
    assert_eq!(
        written.lines().count(),
        3,
        "header plus one row per grid point"
    );
}

// ------------------------------------------------------------- the quiet path

/// The overwhelmingly common input — one symbol, one cadence, no `freq` column
/// at all — is untouched: no warning, no error, and the same numbers as before.
#[test]
fn a_plain_single_cadence_input_says_nothing() {
    let out = Cmd::new("run")
        .arg(&common::cli::at("examples/strategy.yml"))
        .series(&common::cli::at("examples/candles.csv"))
        .args(&["--crypto", "-f", "1d"])
        .output_dir("cadence_plain")
        .ok();
    assert!(
        !out.stderr.contains("cadence"),
        "a plain input produced a cadence finding:\n{}",
        out.stderr
    );
}

/// A `time` column in **nanoseconds** — what `datetime64[ns]` cast to an
/// integer produces, and the shape a `pandas`/`polars` export lands in.
///
/// Every stamp is then ~52 million years past the epoch. `time` tops out at
/// year 9999, so the first `!is_weekday` in the document used to kill the run
/// with a raw `expect` message from `Timestamp::to_datetime`; making that
/// total turned the abort into a strategy that silently never fires — which is
/// worse, because it looks like a result. The census says so instead.
#[test]
fn nanosecond_timestamps_are_named_rather_than_silently_ungated() {
    // 2024-01-01 onward, one day apart, in nanoseconds.
    const NS_PER_DAY: i64 = 86_400_000_000_000;
    let base = 1_704_067_200_000_000_000i64;
    let mut body = String::new();
    for i in 0..40i64 {
        let close = 100 + (i % 7) * 2;
        body.push_str(&format!(
            "BTC,1d,{},{close},{close},{close},{close},100\n",
            base + i * NS_PER_DAY
        ));
    }
    let out = Cmd::new("run")
        .arg(&single_strategy("BTC"))
        .series(&series_of("cadence_nanos.csv", &body))
        .output_dir("cadence_nanos")
        .ok();

    assert!(
        out.stderr.contains("fall outside the calendar"),
        "no undatable-timestamp warning:\n{}",
        out.stderr
    );
    assert!(
        out.stderr.contains("nanoseconds"),
        "the warning should name the usual cause:\n{}",
        out.stderr
    );
    // And the run completes rather than aborting.
    assert!(out.wrote("metrics.yml"), "the run should still complete");

    // The same series in milliseconds is ordinary, and says nothing.
    let mut ms_body = String::new();
    for i in 0..40i64 {
        let close = 100 + (i % 7) * 2;
        ms_body.push_str(&format!(
            "BTC,1d,{},{close},{close},{close},{close},100\n",
            base / 1_000_000 + i * (NS_PER_DAY / 1_000_000)
        ));
    }
    let out = Cmd::new("run")
        .arg(&single_strategy("BTC"))
        .series(&series_of("cadence_millis.csv", &ms_body))
        .output_dir("cadence_millis")
        .ok();
    assert!(
        !out.stderr.contains("fall outside the calendar"),
        "an ordinary millisecond series must not be accused:\n{}",
        out.stderr
    );
}

// ------------------------------------------------------- the `time` column

/// **Regression.** A column named `time` promises timestamps. When none of its
/// values are one, the column is not a time column, and everything
/// time-denominated used to go quiet without failing: carry charged nothing,
/// the calendar leaves read `None` on every bar, and `bars_per_year` had no
/// span to measure. The run completed and reported a strategy that was never
/// charged for carry.
#[test]
fn a_time_column_that_never_parses_stops_the_run() {
    let body: String = (1..=12)
        .map(|i| format!("BTC,1d,bucket-{i:03},{c},{c},{c},{c},100\n", c = 100 + i))
        .collect();
    let out = Cmd::new("run")
        .arg(&single_strategy("BTC"))
        .series(&series_of("cadence_untimed.csv", &body))
        .output_dir("cadence_untimed")
        .fails();

    assert!(
        out.stderr.contains("parse as a timestamp"),
        "no untimed error:\n{}",
        out.stderr
    );
    // Names the offender and the actual remedy — these are not dates, so the
    // column they belong in is `index`.
    assert!(out.stderr.contains("bucket-001"), "{}", out.stderr);
    assert!(
        out.stderr.contains("`index`"),
        "does not point at `index`:\n{}",
        out.stderr
    );
}

/// The same values under `index` are exactly what that column is for, so the
/// run proceeds — refusing above has a remedy, not just a complaint.
#[test]
fn the_same_values_under_index_run_fine() {
    let header = "symbol,index,open,high,low,close,volume\n";
    let body: String = (1..=12)
        .map(|i| format!("BTC,{i},{c},{c},{c},{c},100\n", c = 100 + i))
        .collect();
    let (_, arg) = scratch_file("cadence_indexed.csv", &format!("{header}{body}"));
    let out = Cmd::new("run")
        .arg(&single_strategy("BTC"))
        .series(&arg)
        .arg("--bars-per-year")
        .arg("252")
        .output_dir("cadence_indexed")
        .ok();
    assert!(
        out.stderr.contains("index-sampled"),
        "should say what the input is:\n{}",
        out.stderr
    );
}

/// A `time` that parses for *some* rows is disagreement, not ambiguity: the run
/// is still well-defined, just quietly missing time on those bars. Warned, and
/// the run proceeds.
#[test]
fn a_partly_parseable_time_column_warns_but_runs() {
    let mut body = daily_series("BTC", 12);
    body.push_str("BTC,1d,not-a-date,113,113,113,113,100\n");
    let out = Cmd::new("run")
        .arg(&single_strategy("BTC"))
        .series(&series_of("cadence_partly_timed.csv", &body))
        .output_dir("cadence_partly_timed")
        .ok();

    assert!(
        out.stderr.contains("do not parse as a timestamp"),
        "no partial-time warning:\n{}",
        out.stderr
    );
    assert!(out.stderr.contains("not-a-date"), "{}", out.stderr);
}
