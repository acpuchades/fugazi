//! Cross-asset composition through `Snapshot<Sym>` + `Selector<Sym>` + `Pick`.
//!
//! Proves the point of the source-generic-leaves + list-backed-snapshot
//! refactor: with a multi-asset input frame (`Snapshot<Sym>`) and the
//! [`Pick`] projection, every source-generic candle leaf composes verbatim
//! on top of a specific asset, and arithmetic between two picks is just an
//! arithmetic-over-reals indicator whose `Input` is `Snapshot<Sym>`.

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

fn snap(
    pairs: &[(Option<String>, Option<Frequency>, Atom)],
) -> Snapshot<String> {
    let mut s = Snapshot::new();
    for (sym, freq, a) in pairs {
        s.push(sym.clone(), *freq, a.clone());
    }
    s
}

fn s(sym: &str) -> Option<String> {
    Some(sym.to_string())
}

const T0: i64 = 1_710_506_096_000; // 2024-03-15 12:34:56 UTC — a Friday.

#[test]
fn pick_projects_the_named_asset() {
    let mut btc = Pick::<String>::matching(Selector::by_symbol("BTC"));
    let s_ = snap(&[
        (s("BTC"), None, atom(100.0)),
        (s("ETH"), None, atom(50.0)),
    ]);
    let out = btc.update(s_).expect("BTC present");
    assert_eq!(out.candle.close, 100.0);
}

#[test]
fn close_of_pick_reads_the_projected_close() {
    let mut btc_close =
        Close::of(Pick::<String>::matching(Selector::by_symbol("BTC")));
    let s_ = snap(&[
        (s("BTC"), None, atom(101.5)),
        (s("ETH"), None, atom(4.25)),
    ]);
    assert_eq!(btc_close.update(s_), Some(101.5));
}

#[test]
fn btc_eth_close_spread_composes_from_two_picks() {
    let mut spread =
        Close::of(Pick::<String>::matching(Selector::by_symbol("BTC")))
            .sub(Close::of(Pick::<String>::matching(Selector::by_symbol("ETH"))));
    let s_ = snap(&[
        (s("BTC"), None, atom(100.0)),
        (s("ETH"), None, atom(60.0)),
    ]);
    assert_eq!(spread.update(s_), Some(40.0));
}

#[test]
fn bar_indicator_stacks_on_a_pick() {
    let mut atr = Atr::new(
        CurrentBar::of(Pick::<String>::matching(Selector::by_symbol("BTC"))),
        3,
    );
    for i in 0..10 {
        let s_ = snap(&[
            (s("BTC"), None, atom(100.0 + i as Real)),
            (s("ETH"), None, atom(60.0 - i as Real)),
        ]);
        atr.update(s_);
    }
    assert!(atr.value().is_some());
}

#[test]
fn calendar_source_over_pick_reads_projected_time() {
    let mut year = Year::of(Pick::<String>::matching(Selector::by_symbol("BTC")));
    let s_ = snap(&[
        (s("BTC"), None, atom_at(T0, 100.0)),
        (s("ETH"), None, atom_at(T0 + 60_000, 50.0)),
    ]);
    assert_eq!(year.update(s_), Some(2024.0));
}

#[test]
fn missing_asset_stays_none_downstream() {
    let mut spread =
        Close::of(Pick::<String>::matching(Selector::by_symbol("BTC")))
            .sub(Close::of(Pick::<String>::matching(Selector::by_symbol("ETH"))));
    let s_ = snap(&[
        (s("SOL"), None, atom(20.0)),
        (s("ETH"), None, atom(10.0)),
    ]);
    assert_eq!(spread.update(s_), None);
}

#[test]
fn pick_by_freq_wildcards_symbol() {
    let mut hourly =
        Pick::<String>::matching(Selector::by_freq(Frequency::Hour(1)));
    let s_ = snap(&[
        (s("BTC"), Some(Frequency::Hour(1)), atom(100.0)),
        (s("ETH"), Some(Frequency::Day(1)), atom(50.0)),
    ]);
    let out = hourly.update(s_).expect("some hourly entry");
    assert_eq!(out.candle.close, 100.0);
}

#[test]
fn pick_exact_disambiguates_between_frequencies() {
    let mut btc_hour = Pick::<String>::matching(Selector::exact(
        "BTC",
        Frequency::Hour(1),
    ));
    let s_ = snap(&[
        (s("BTC"), Some(Frequency::Hour(1)), atom(100.0)),
        (s("BTC"), Some(Frequency::Day(1)), atom(300.0)),
    ]);
    assert_eq!(btc_hour.update(s_).map(|a| a.candle.close), Some(100.0));
}

#[test]
fn empty_selector_unpacks_a_single_entry_snapshot() {
    // Single-series ergonomics: an empty Selector on Pick unpacks the sole atom.
    let mut close = Close::of(Pick::<String>::new());
    // Tagged size-1 works.
    let s_ = snap(&[(s("BTC"), None, atom(42.0))]);
    assert_eq!(close.update(s_), Some(42.0));
    // Untagged size-1 (Snapshot::of_atom / From<Atom>) also works.
    close.reset();
    let s_ = Snapshot::<String>::of_atom(atom(7.0));
    assert_eq!(close.update(s_), Some(7.0));
}

#[test]
#[should_panic(expected = "Snapshot::sole_atom: expected a single-entry snapshot")]
fn empty_selector_panics_on_multi_entry_snapshot() {
    // The loud-failure guard: a no-query Pick fed a multi-asset snapshot is
    // almost always a wiring bug — panic rather than silently pick.
    let mut close = Close::of(Pick::<String>::new());
    close.update(snap(&[
        (s("BTC"), None, atom(100.0)),
        (s("ETH"), None, atom(60.0)),
    ]));
}

#[test]
fn atom_equality_is_by_time() {
    // The tie-in with the PartialEq-by-time impl: two atoms with the same
    // timestamp compare equal regardless of prices.
    assert_eq!(atom_at(T0, 1.0), atom_at(T0, 9999.0));
    assert_ne!(atom_at(T0, 1.0), atom_at(T0 + 1, 1.0));
    assert_eq!(atom(1.0), atom(2.0));
}

#[test]
fn atoms_sort_chronologically() {
    let mut atoms = [
        atom_at(T0 + 200, 1.0),
        atom_at(T0, 1.0),
        atom_at(T0 + 100, 1.0),
        atom(1.0),
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
