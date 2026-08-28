//! The five **trailing strategy risk** tags — `!sharpe`, `!sortino`,
//! `!volatility`, `!max_drawdown`, `!calmar` — and the `strategy:` slot they
//! share.
//!
//! These are the crate's only expression tags that embed a whole strategy
//! document plus a private wallet, and they had no test at all: four of the five
//! appeared nowhere in the suite, and `tests/spec_grammar.rs` skipped all five
//! outright (its probe had no stand-in for a `strategy` slot, so every declared
//! form went unparsed). Wiring `!sortino` to build a `Sharpe`, `!calmar` to
//! build a `Volatility` and `!max_drawdown` to build a `Volatility` all left the
//! suite green.
//!
//! Two things are pinned here, and they are different questions:
//!
//! - **Which indicator each tag builds** — checked against the library
//!   indicator constructed directly over the same stream. The indicators' own
//!   arithmetic is `src/indicators/trailing.rs`'s unit tests' job; this is the
//!   `TrailingMetric` → constructor mapping in `src/spec/trailing.rs`.
//! - **What the shared `strategy:` slot accepts** — `AnyStrategyRef` routes a
//!   preset, a full single-asset map, a pair, a basket and a multi, and only
//!   the single-asset arm was ever exercised (by one `!sharpe` in
//!   `tests/resume.rs`).

mod common;

use std::sync::Arc;

use fugazi::indicators::{Calmar, MaxDrawdown, Sharpe, Sortino, Volatility};
use fugazi::prelude::*;
use fugazi::runtime::{PayloadIndicator, PayloadValue};
use fugazi::spec::overlay::build_overlay;
use fugazi::spec::{NodeSpec, Root};
use fugazi::strategies::SingleAssetStrategy;
use fugazi::types::{Snapshot, Symbol, symbol as intern};

const SYMBOL: &str = "X";
const PERIOD: usize = 10;
const BPY: Real = 365.0;

/// The wallet and book seed `src/spec/trailing.rs` gives every embedded
/// strategy. Every metric here is a ratio of equity-curve *returns* and a
/// buy-and-hold strategy reads no book, so the comparison below is invariant to
/// this value — it is spelled out to match, not depended on.
const SEED: Real = 1_000.0;

/// A path that rises, falls hard enough to draw a real drawdown, and rises
/// again — so Sharpe and Sortino differ in sign of contribution, the drawdown is
/// non-zero (Calmar and `!max_drawdown` need it), and volatility is not
/// degenerate.
fn stream() -> Vec<Snapshot<Symbol>> {
    let mut closes: Vec<Real> = (0..30).map(|i| 100.0 + f64::from(i) * 2.0).collect();
    closes.extend((1..=20).map(|i| 158.0 - f64::from(i) * 3.0));
    closes.extend((1..=30).map(|i| 98.0 + f64::from(i) * 1.5));
    common::bars::series(SYMBOL, &closes, common::bars::flat)
}

/// Build the expression `yaml` as an overlay column and read it once per bar.
fn readings(yaml: &str) -> Vec<Option<Real>> {
    let spec: NodeSpec = serde_norway::from_str(yaml).expect("expression parses");
    let mut ind = build_overlay(&spec, &Schema::empty(), Root::sole()).expect("expression builds");
    drive(&mut *ind)
}

fn drive(ind: &mut dyn PayloadIndicator) -> Vec<Option<Real>> {
    stream()
        .into_iter()
        .map(|snap| match ind.update(PayloadValue::Snapshot(snap)) {
            Some(PayloadValue::Real(v)) => Some(v),
            None => None,
            other => panic!("a trailing risk tag must read as a scalar, got {other:?}"),
        })
        .collect()
}

/// The same readings from a library indicator built by hand over the same
/// strategy — the reference side of the comparison.
fn direct<I>(mut ind: I) -> Vec<Option<Real>>
where
    I: Indicator<Input = Snapshot<Symbol>, Output = Real>,
{
    stream().into_iter().map(|s| ind.update(s)).collect()
}

fn buy_and_hold() -> SingleAssetStrategy<Symbol> {
    SingleAssetStrategy::buy_and_hold(intern(SYMBOL))
}

/// `!<tag> { strategy: !buy_and_hold { root: X }, … }`, with whichever of
/// `bars_per_year` the tag takes.
fn tag(name: &str, extra: &str) -> String {
    format!("!{name} {{ strategy: !buy_and_hold {{ root: {SYMBOL} }}, period: {PERIOD}{extra} }}")
}

/// **Each tag builds the indicator it is named for.** Bar for bar, against the
/// library type constructed directly.
///
/// The comparison is exact rather than approximate: both sides are the same
/// code over the same stream, so any difference is a wiring difference.
#[test]
fn each_trailing_tag_builds_the_indicator_it_names() {
    let sym = intern(SYMBOL);
    /// `(tag name, what the tag built, what the library type reads)`.
    type Case = (&'static str, Vec<Option<Real>>, Vec<Option<Real>>);
    let cases: Vec<Case> = vec![
        (
            "sharpe",
            readings(&tag("sharpe", &format!(", bars_per_year: {BPY}"))),
            direct(Sharpe::new(
                buy_and_hold(),
                sym.clone(),
                SEED,
                PERIOD,
                0.0,
                BPY,
            )),
        ),
        (
            "sortino",
            readings(&tag("sortino", &format!(", bars_per_year: {BPY}"))),
            direct(Sortino::new(
                buy_and_hold(),
                sym.clone(),
                SEED,
                PERIOD,
                0.0,
                BPY,
            )),
        ),
        (
            "volatility",
            readings(&tag("volatility", &format!(", bars_per_year: {BPY}"))),
            direct(Volatility::new(
                buy_and_hold(),
                sym.clone(),
                SEED,
                PERIOD,
                BPY,
            )),
        ),
        (
            "max_drawdown",
            readings(&tag("max_drawdown", "")),
            direct(MaxDrawdown::new(buy_and_hold(), sym.clone(), SEED, PERIOD)),
        ),
        (
            "calmar",
            readings(&tag("calmar", &format!(", bars_per_year: {BPY}"))),
            direct(Calmar::new(buy_and_hold(), sym.clone(), SEED, PERIOD, BPY)),
        ),
    ];

    for (name, got, want) in &cases {
        assert_eq!(got, want, "!{name} did not build a {name}");
        assert!(
            got.iter().any(Option::is_some),
            "!{name} never produced a reading over the path, so the comparison \
             above is vacuous"
        );
    }

    // Five identical columns would satisfy every assertion above, so require the
    // five to actually differ from each other over this path.
    for (i, (a, ga, _)) in cases.iter().enumerate() {
        for (b, gb, _) in &cases[i + 1..] {
            assert_ne!(ga, gb, "!{a} and !{b} read identically over the path");
        }
    }
}

/// `risk_free_rate` is optional and defaults to zero — and it is the one knob
/// two of the five tags read and the other three ignore.
#[test]
fn the_risk_free_rate_defaults_to_zero_and_is_read_when_given() {
    let with_rf = readings(&format!(
        "!sharpe {{ strategy: !buy_and_hold {{ root: {SYMBOL} }}, period: {PERIOD}, \
         bars_per_year: {BPY}, risk_free_rate: 0.5 }}"
    ));
    let without = readings(&tag("sharpe", &format!(", bars_per_year: {BPY}")));
    let explicit_zero = readings(&format!(
        "!sharpe {{ strategy: !buy_and_hold {{ root: {SYMBOL} }}, period: {PERIOD}, \
         bars_per_year: {BPY}, risk_free_rate: 0.0 }}"
    ));

    assert_eq!(
        without, explicit_zero,
        "the default must be an explicit zero"
    );
    assert_ne!(with_rf, without, "a non-zero rate must move the reading");
}

/// **The `strategy:` slot takes all four shapes.** `AnyStrategyRef` routes a
/// preset or single-asset map, a pair, a basket and a multi, and each arm
/// deserializes through a different path (the pairs / basket / multi arms go via
/// the `serde_json` bridge, the single-asset one does not).
///
/// A basket / multi is fed one tagged symbol per bar here, which is a thin diet
/// for a cross-sectional shape — the assertion is that the arm *builds and
/// runs*, which is what a wrong route would break.
#[test]
fn the_strategy_slot_routes_every_shape() {
    let bodies = [
        ("preset", format!("!buy_and_hold {{ root: {SYMBOL} }}")),
        (
            "single-asset map",
            format!("{{ root: {SYMBOL}, long: {{ enter: !value true, exit: !value false }} }}"),
        ),
        (
            "pairs",
            format!(
                "{{ left: {SYMBOL}, right: {SYMBOL}, long_spread: {{ \
                 enter: !value true, exit: !value false }} }}"
            ),
        ),
        (
            "basket",
            "{ score: !close { source: !pick { symbol: !slot SYM } }, \
              sizing: !value 1.0, \
              selection: !top_bottom { longs: 1, shorts: 0 } }"
                .to_string(),
        ),
        (
            "multi",
            // A multi is the shape that declares no traded series upfront, so
            // it is defined by the *absence* of `root:` / `left:` / `selection:`.
            "{ long: { enter: !value true, exit: !value false } }".to_string(),
        ),
    ];

    for (shape, body) in bodies {
        let yaml =
            format!("!sharpe {{ strategy: {body}, period: {PERIOD}, bars_per_year: {BPY} }}");
        let spec: NodeSpec = serde_norway::from_str(&yaml)
            .unwrap_or_else(|e| panic!("the {shape} arm must parse: {e}\n{yaml}"));
        let mut ind = build_overlay(&spec, &Schema::empty(), Root::sole())
            .unwrap_or_else(|e| panic!("the {shape} arm must build: {e}\n{yaml}"));
        let got = drive(&mut *ind);
        assert!(
            got.iter().any(Option::is_some),
            "the {shape} arm built but never produced a reading"
        );
    }
}

/// A malformed embedded document is bad **input**: it must come back as an
/// error carrying the enclosing tag's breadcrumb, not abort the process.
#[test]
fn a_malformed_embedded_strategy_is_an_error_naming_the_tag() {
    let yaml = format!(
        "!sharpe {{ strategy: {{ root: {SYMBOL}, sizing: !get {{ key: nope }} }}, \
         period: {PERIOD}, bars_per_year: {BPY} }}"
    );
    let spec: NodeSpec = serde_norway::from_str(&yaml).expect("the document parses");
    let err = match build_overlay(&spec, &Schema::empty(), Root::sole()) {
        Err(e) => e.to_string(),
        Ok(_) => panic!("an unknown overlay column cannot build"),
    };
    assert!(
        err.contains("!sharpe"),
        "the error must name the tag the failure came from: {err}"
    );
    assert!(
        err.contains("nope"),
        "the error must name the unknown column: {err}"
    );
}

/// `period` is a `positive_uint`, and zero is refused rather than dividing by a
/// zero-length window mid-run.
#[test]
fn a_zero_period_is_refused() {
    let yaml = format!(
        "!sharpe {{ strategy: !buy_and_hold {{ root: {SYMBOL} }}, period: 0, bars_per_year: {BPY} }}"
    );
    let built = serde_norway::from_str::<NodeSpec>(&yaml)
        .map(|spec| build_overlay(&spec, &Schema::empty(), Root::sole()).map(|_| ()));
    assert!(
        matches!(built, Err(_) | Ok(Err(_))),
        "period: 0 must be refused at parse or at build, not accepted"
    );
}

/// The overlay schema reaches the embedded document: a `!get` inside the
/// strategy resolves against the schema the enclosing column was built with.
#[test]
fn the_schema_reaches_the_embedded_strategy() {
    let mut b = Schema::builder();
    b.add_bool("risk_on");
    let schema: Arc<Schema> = b.finish();

    let yaml = format!(
        "!sharpe {{ strategy: {{ root: {SYMBOL}, long: {{ enter: !get {{ key: risk_on }}, \
         exit: !value false }} }}, period: {PERIOD}, bars_per_year: {BPY} }}"
    );
    let spec: NodeSpec = serde_norway::from_str(&yaml).expect("parses");
    build_overlay(&spec, &schema, Root::sole())
        .expect("a `!get` inside the embedded strategy resolves against the outer schema");

    // …and the same document fails against a schema without the column, so the
    // success above is the schema being threaded rather than the key going
    // unchecked.
    assert!(
        build_overlay(&spec, &Schema::empty(), Root::sole()).is_err(),
        "an unknown key must still fail"
    );
}
