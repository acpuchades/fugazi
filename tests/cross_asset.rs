//! Cross-asset composition through `Snapshot<Selector>` + `Pick`.
//!
//! Proves the point of the source-generic-leaves refactor + the Selector
//! surface: with a multi-asset input frame (`Snapshot<Selector>`) and the
//! [`Pick`] projection, every source-generic candle leaf composes verbatim on
//! top of a specific asset, and arithmetic between two picks is just an
//! arithmetic-over-reals indicator whose `Input` is `Snapshot<Selector>`.

use fugazi::indicator::Indicator;
use fugazi::indicators::{Atr, Close, CurrentBar, Pick, Year};
use fugazi::prelude::*;
use fugazi::{Frequency, Selector, Snapshot, Timestamp};

fn atom_at(ms: i64, close: Real) -> Atom {
    Atom::with_time(Candle::new(1.0, 2.0, 0.5, close, 100.0), Timestamp(ms))
}

fn atom(close: Real) -> Atom {
    Atom::new(Candle::new(1.0, 2.0, 0.5, close, 100.0))
}

fn snap(pairs: &[(Selector, Atom)]) -> Snapshot<Selector> {
    pairs.iter().map(|(k, a)| (k.clone(), a.clone())).collect()
}

const T0: i64 = 1_710_506_096_000; // 2024-03-15 12:34:56 UTC — a Friday.

#[test]
fn pick_projects_the_named_asset() {
    let mut btc = Pick::matching(Selector::by_symbol("BTC"));
    let s = snap(&[
        (Selector::by_symbol("BTC"), atom(100.0)),
        (Selector::by_symbol("ETH"), atom(50.0)),
    ]);
    let out = btc.update(s).expect("BTC present");
    assert_eq!(out.candle.close, 100.0);
}

#[test]
fn close_of_pick_reads_the_projected_close() {
    // `Close::of(Pick::matching(Selector::by_symbol("BTC")))` = "BTC's close"
    // as a Real indicator whose Input is `Snapshot<Selector>`.
    let mut btc_close = Close::of(Pick::matching(Selector::by_symbol("BTC")));
    let s = snap(&[
        (Selector::by_symbol("BTC"), atom(101.5)),
        (Selector::by_symbol("ETH"), atom(4.25)),
    ]);
    assert_eq!(btc_close.update(s), Some(101.5));
}

#[test]
fn btc_eth_close_spread_composes_from_two_picks() {
    // The headline expression the refactor exists to support.
    let mut spread = Close::of(Pick::matching(Selector::by_symbol("BTC")))
        .sub(Close::of(Pick::matching(Selector::by_symbol("ETH"))));
    let s = snap(&[
        (Selector::by_symbol("BTC"), atom(100.0)),
        (Selector::by_symbol("ETH"), atom(60.0)),
    ]);
    assert_eq!(spread.update(s), Some(40.0));
}

#[test]
fn bar_indicator_stacks_on_a_pick() {
    // `Atr` takes `S: Indicator<Output = Candle>`; wrapping a Pick in
    // `CurrentBar::of(...)` re-projects Atom → Candle, so an ATR over BTC
    // reads the full bar out of the snapshot.
    let mut atr = Atr::new(
        CurrentBar::of(Pick::matching(Selector::by_symbol("BTC"))),
        3,
    );
    for i in 0..10 {
        let s = snap(&[
            (Selector::by_symbol("BTC"), atom(100.0 + i as Real)),
            (Selector::by_symbol("ETH"), atom(60.0 - i as Real)),
        ]);
        atr.update(s);
    }
    // ATR(3) warms up on bar 3 → the value should be Some by now.
    assert!(atr.value().is_some());
}

#[test]
fn calendar_source_over_pick_reads_projected_time() {
    // Feed the calendar accessor an atom projected out of the snapshot — the
    // asset's own bar-open time is what gets decomposed.
    let mut year = Year::of(Pick::matching(Selector::by_symbol("BTC")));
    let s = snap(&[
        (Selector::by_symbol("BTC"), atom_at(T0, 100.0)),
        (Selector::by_symbol("ETH"), atom_at(T0 + 60_000, 50.0)),
    ]);
    assert_eq!(year.update(s), Some(2024.0));
}

#[test]
fn missing_asset_stays_none_downstream() {
    // BTC is not present — Close::of(Pick("BTC")) emits None, and the
    // downstream Sub is None too (both operands must be Some).
    let mut spread = Close::of(Pick::matching(Selector::by_symbol("BTC")))
        .sub(Close::of(Pick::matching(Selector::by_symbol("ETH"))));
    let s = snap(&[
        (Selector::by_symbol("SOL"), atom(20.0)),
        (Selector::by_symbol("ETH"), atom(10.0)),
    ]);
    assert_eq!(spread.update(s), None);
}

#[test]
fn pick_by_freq_wildcards_symbol() {
    // Query on freq only; storage carries both fields.
    let mut hourly = Pick::matching(Selector::by_freq(Frequency::Hour(1)));
    let s = snap(&[
        (Selector::exact("BTC", Frequency::Hour(1)), atom(100.0)),
        (Selector::exact("ETH", Frequency::Day(1)), atom(50.0)),
    ]);
    let out = hourly.update(s).expect("some hourly entry");
    assert_eq!(out.candle.close, 100.0);
}

#[test]
fn pick_exact_disambiguates_between_frequencies() {
    let mut btc_hour = Pick::matching(Selector::exact("BTC", Frequency::Hour(1)));
    let s = snap(&[
        (Selector::exact("BTC", Frequency::Hour(1)), atom(100.0)),
        (Selector::exact("BTC", Frequency::Day(1)), atom(300.0)),
    ]);
    assert_eq!(btc_hour.update(s).map(|a| a.candle.close), Some(100.0));
}

#[test]
fn empty_selector_unpacks_a_single_entry_snapshot() {
    // Single-series ergonomics: an empty Selector on Pick means "there'd
    // better be exactly one entry", and its atom is what comes out.
    let mut close = Close::of(Pick::new());
    let s = snap(&[(Selector::by_symbol("BTC"), atom(42.0))]);
    assert_eq!(close.update(s), Some(42.0));
}

#[test]
#[should_panic(expected = "Snapshot::sole_atom: expected a single-entry snapshot")]
fn empty_selector_panics_on_multi_entry_snapshot() {
    // The loud-failure guard: a no-query Pick fed a multi-asset snapshot is
    // almost always a wiring bug — panic rather than silently pick.
    let mut close = Close::of(Pick::new());
    close.update(snap(&[
        (Selector::by_symbol("BTC"), atom(100.0)),
        (Selector::by_symbol("ETH"), atom(60.0)),
    ]));
}

#[test]
fn atom_equality_is_by_time() {
    // The tie-in with the PartialEq-by-time impl: two atoms with the same
    // timestamp compare equal regardless of prices.
    assert_eq!(atom_at(T0, 1.0), atom_at(T0, 9999.0));
    assert_ne!(atom_at(T0, 1.0), atom_at(T0 + 1, 1.0));
    // Undated atoms are all equal to each other under the `None` convention.
    assert_eq!(atom(1.0), atom(2.0));
}

#[test]
fn atoms_sort_chronologically() {
    let mut atoms = [
        atom_at(T0 + 200, 1.0),
        atom_at(T0, 1.0),
        atom_at(T0 + 100, 1.0),
        atom(1.0), // undated → sorts first
    ];
    atoms.sort();
    let times: Vec<Option<Timestamp>> = atoms.iter().map(|a| a.time).collect();
    assert_eq!(
        times,
        [
            None,
            Some(Timestamp(T0)),
            Some(Timestamp(T0 + 100)),
            Some(Timestamp(T0 + 200)),
        ]
    );
}
