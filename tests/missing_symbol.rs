//! A declared symbol that is absent from a *bar* versus absent from the
//! *stream* — two failures that used to be one.
//!
//! Both cases previously fell through `Snapshot::sole_atom_or_panic`. A symbol with a
//! shorter history (a late listing, a delisting, a holiday) panicked the run
//! from `strategies::single_asset::extract_self_atom` — the Position router,
//! not a leaf, which is why the panic's "add `!pick` to each leaf" advice did
//! not help and a document with fully explicit `!pick` leaves failed
//! identically. A symbol absent from the whole stream did the opposite: on a
//! single-entry stream it silently unpacked an unrelated series and completed
//! as a zero-fill, fully-metricked "successful" run.
//!
//! The first is ordinary and must not fail; the second is bad input and must
//! fail once, up front, by name.

use fugazi::backtest::Closeout;
use fugazi::spec::backtest::{
    EvalContext, run_iteration_any, run_iteration_resumable, validate_universe,
};
use fugazi::spec::costs::CostConfig;
use fugazi::spec::preset::StrategyRef;
use fugazi::spec::{SingleStrategySpec, StrategySpec};
use fugazi::types::{Atom, Candle, Snapshot, Symbol, Timestamp, symbol};

const N_SYMS: usize = 9;
/// The bar `S8USDT` first quotes on.
const LISTS_AT: usize = 120;
const BARS: usize = 300;

/// Oscillating, so a crossover document actually changes state — a
/// monotonic ramp never crosses back and every fill assertion below would be
/// vacuously true.
fn price(bar: usize, k: usize) -> f64 {
    100.0 + k as f64 + 10.0 * ((bar as f64) / 7.0).sin()
}

fn atom(bar: usize, px: f64) -> Atom {
    Atom::with_time(
        Candle::new(px, px + 1.0, px - 1.0, px, 100.0),
        Timestamp(bar as i64 * 60_000),
    )
}

/// Nine symbols; `S8USDT` has no atom before [`LISTS_AT`], all nine from there on.
fn late_listing_stream() -> Vec<Snapshot<Symbol>> {
    (0..BARS)
        .map(|i| {
            let mut snap = Snapshot::<Symbol>::default();
            for k in 0..N_SYMS {
                if k == N_SYMS - 1 && i < LISTS_AT {
                    continue;
                }
                snap.push(
                    Some(symbol(format!("S{k}USDT"))),
                    None,
                    atom(i, price(i, k)),
                );
            }
            snap
        })
        .collect()
}

fn single(text: &str) -> StrategySpec {
    let spec = SingleStrategySpec::from_text_with_params_in(
        text,
        &Default::default(),
        std::path::Path::new("."),
        std::path::Path::new("."),
        "test",
    )
    .expect("document parses");
    StrategySpec::Single(Box::new(StrategyRef::Spec(Box::new(spec))))
}

/// A crossover that trades on almost every regime change, so "did it advance"
/// is visible in the fill count rather than inferred.
fn doc(sym: &str) -> String {
    format!(
        "root: {sym}\n\
         long:\n  \
           enter: !crosses_above {{ lhs: !close, rhs: !sma {{ period: 10 }} }}\n  \
           exit: !crosses_below {{ lhs: !close, rhs: !sma {{ period: 10 }} }}\n"
    )
}

fn ctx(costs: &CostConfig) -> EvalContext<'_> {
    EvalContext {
        cash: 10_000.0,
        max_gross: None,
        leverage: 1.0,
        margin_rate: 0.0,
        maintenance_margin: None,
        bars_per_year: 365.0,
        risk_free_rate: 0.0,
        cost_config: costs,
        effective_freq: None,
        stream: None,
        windowed: None,
        seconds_per_bar: None,
        mc: None,
        warmup_bars: None,
    }
}

fn labels(n: usize) -> Vec<String> {
    (0..n).map(|i| i.to_string()).collect()
}

// ---------------------------------------------------------------------------
// Absent from a bar — ordinary, must not fail
// ---------------------------------------------------------------------------

#[test]
fn a_late_listing_runs_instead_of_panicking() {
    // Was: `PanicException` / `panic!("... got 8 entries")` on the first bar,
    // because the other eight symbols were present and the declared one wasn't.
    let snaps = late_listing_stream();
    let costs = empty_costs();
    let iter = run_iteration_any(
        &single(&doc("S8USDT")),
        labels(snaps.len()),
        &snaps,
        &ctx(&costs),
    )
    .expect("a late listing is ordinary input, not an error");
    assert_eq!(
        iter.report.equity_curve.len(),
        BARS,
        "every bar is still evaluated"
    );
}

#[test]
fn explicit_pick_leaves_run_too() {
    // The old panic told you to name the asset on every leaf. Measured, that
    // did not help: the panic came from the Position router, which reads the
    // *declared* symbol and never looks at a leaf. This is that document.
    let text = "root: S8USDT\n\
                long:\n  \
                  enter: !crosses_above\n    \
                    lhs: !close { source: !pick { symbol: S8USDT } }\n    \
                    rhs: !sma { period: 10, source: !close { source: !pick { symbol: S8USDT } } }\n  \
                  exit: !crosses_below\n    \
                    lhs: !close { source: !pick { symbol: S8USDT } }\n    \
                    rhs: !sma { period: 10, source: !close { source: !pick { symbol: S8USDT } } }\n";
    let snaps = late_listing_stream();
    let costs = empty_costs();
    assert!(
        run_iteration_any(&single(text), labels(snaps.len()), &snaps, &ctx(&costs)).is_ok(),
        "explicit `!pick` leaves are not what this ever depended on"
    );
}

#[test]
fn the_series_does_not_advance_before_it_lists() {
    // "Does not advance" is the contract: no orders, and equity marks against
    // the last known price — which, before the first quote, is the seed.
    let snaps = late_listing_stream();
    let costs = empty_costs();
    let iter = run_iteration_any(
        &single(&doc("S8USDT")),
        labels(snaps.len()),
        &snaps,
        &ctx(&costs),
    )
    .expect("runs");

    assert!(
        iter.report.fills.iter().all(|f| f.bar >= LISTS_AT),
        "a fill was booked on a bar the declared symbol had not listed on yet"
    );
    assert!(
        iter.report.equity_curve[..LISTS_AT]
            .iter()
            .all(|e| (*e - iter.report.initial_equity).abs() < 1e-9),
        "equity moved before the declared symbol ever quoted"
    );
    // And it does eventually trade, or the assertions above are vacuous.
    assert!(
        !iter.report.fills.is_empty(),
        "never traded at all — the test proves nothing"
    );
}

#[test]
fn a_symbol_present_from_the_first_bar_is_unaffected() {
    let snaps = late_listing_stream();
    let costs = empty_costs();
    let iter = run_iteration_any(
        &single(&doc("S0USDT")),
        labels(snaps.len()),
        &snaps,
        &ctx(&costs),
    )
    .expect("runs");
    assert_eq!(iter.report.equity_curve.len(), BARS);
    assert!(!iter.report.fills.is_empty());
}

// ---------------------------------------------------------------------------
// Absent from the stream — bad input, must fail by name
// ---------------------------------------------------------------------------

#[test]
fn a_symbol_absent_from_the_whole_stream_is_refused() {
    // Was: 300 bars, 0 fills, a full metrics document, exit 0. The single-entry
    // stream is the sharp case — `sole_atom_or_panic` unpacked the unrelated series
    // rather than panicking, so nothing anywhere said a word.
    let snaps: Vec<Snapshot<Symbol>> = (0..BARS)
        .map(|i| Snapshot::single(symbol("BTCUSDT"), atom(i, price(i, 0))))
        .collect();
    let costs = empty_costs();
    let err = match run_iteration_any(
        &single(&doc("BTCUSD")),
        labels(snaps.len()),
        &snaps,
        &ctx(&costs),
    ) {
        Err(e) => e,
        Ok(_) => panic!("a symbol that is nowhere in the input is not a runnable document"),
    };
    assert!(err.contains("BTCUSD"), "names the declared symbol: {err}");
    assert!(
        err.contains("BTCUSDT"),
        "names what the stream does carry: {err}"
    );
}

#[test]
fn the_check_is_over_the_stream_not_the_bar() {
    // One quote anywhere in the stream is enough — this is the line between the
    // two cases, so it gets its own test.
    let snaps = late_listing_stream();
    validate_universe(&single(&doc("S8USDT")), &snaps).expect("present on bar 120 is present");
    validate_universe(&single(&doc("S9USDT")), &snaps)
        .expect_err("present on no bar at all is absent");
}

#[test]
fn a_resumed_chunk_may_not_quote_the_symbol() {
    // The state carrying the symbol came from an earlier chunk, so a chunk in
    // which it never quotes is legitimate and must not be refused.
    let spec = single(&doc("S8USDT"));
    let costs = empty_costs();
    let full = late_listing_stream();

    // Chunk 1: bars LISTS_AT.. — the symbol quotes, so this is a valid cold start.
    let warm: Vec<Snapshot<Symbol>> = full[LISTS_AT..].to_vec();
    let (_, state) = run_iteration_resumable(
        &spec,
        labels(warm.len()),
        &warm,
        &ctx(&costs),
        None,
        &Closeout::Carry,
    )
    .expect("cold chunk runs");

    // Chunk 2: bars in which S8USDT does not appear at all.
    let quiet: Vec<Snapshot<Symbol>> = full[..LISTS_AT].to_vec();
    assert!(
        run_iteration_resumable(
            &spec,
            labels(quiet.len()),
            &quiet,
            &ctx(&costs),
            Some(&state),
            &Closeout::Carry,
        )
        .is_ok(),
        "a resumed chunk is not required to quote the symbol"
    );

    // The same chunk cold *is* refused — the resume is what makes it legal.
    assert!(
        run_iteration_resumable(
            &spec,
            labels(quiet.len()),
            &quiet,
            &ctx(&costs),
            None,
            &Closeout::Carry,
        )
        .is_err(),
        "cold, that chunk never sees the declared symbol"
    );
}

#[test]
fn discovered_universes_declare_nothing_to_check() {
    // Basket and multi-asset read their universe off the stream, so they have
    // no declaration that can disagree with it.
    let basket = "selection: !top_bottom { longs: 2, shorts: 2 }\n\
                  score: !roc { period: 20, source: !close { source: !pick { symbol: !slot SYM } } }\n\
                  sizing: !equal_weight 4\n";
    let spec = fugazi::spec::BasketStrategySpec::from_text_with_params_in(
        basket,
        &Default::default(),
        std::path::Path::new("."),
        std::path::Path::new("."),
        "test",
    )
    .expect("parses");
    let spec = StrategySpec::Basket(Box::new(spec));
    assert!(spec.declared_symbols().is_empty());
    validate_universe(&spec, &late_listing_stream()).expect("nothing declared, nothing to refuse");
}

#[test]
fn a_pairs_document_declares_both_legs() {
    let text = "left: S0USDT\nright: S9USDT\n\
                enter: !gt { lhs: !close { source: !pick { symbol: S0USDT } }, rhs: !value 0 }\n";
    let spec = fugazi::spec::PairsStrategySpec::from_text_with_params_in(
        text,
        &Default::default(),
        std::path::Path::new("."),
        std::path::Path::new("."),
        "test",
    )
    .expect("parses");
    let spec = StrategySpec::Pairs(Box::new(spec));
    assert_eq!(
        spec.declared_symbols(),
        vec!["S0USDT".to_string(), "S9USDT".to_string()]
    );
    let err = validate_universe(&spec, &late_listing_stream())
        .expect_err("S9USDT is in no bar of that stream");
    assert!(
        err.contains("trades symbol `S9USDT`"),
        "names only the missing leg as the problem, in the singular: {err}"
    );
}

fn empty_costs() -> CostConfig {
    serde_json::from_str("{}").expect("empty cost config")
}
