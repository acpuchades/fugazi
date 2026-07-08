//! Cross-asset composition through `Snapshot` + `Pick`.
//!
//! Proves the whole point of the source-generic-leaves refactor: with a
//! multi-asset input frame (`Snapshot<K>`) and the [`Pick`] projection, every
//! source-generic candle leaf composes verbatim on top of a specific asset,
//! and arithmetic between two picks is just an arithmetic-over-reals
//! indicator whose `Input` is `Snapshot<K>`.

use fugazi::indicator::Indicator;
use fugazi::indicators::{Atr, Close, CurrentBar, Pick, Year};
use fugazi::prelude::*;
use fugazi::{Snapshot, Timestamp};

fn atom_at(ms: i64, close: Real) -> Atom {
    Atom::with_time(Candle::new(1.0, 2.0, 0.5, close, 100.0), Timestamp(ms))
}

fn atom(close: Real) -> Atom {
    Atom::new(Candle::new(1.0, 2.0, 0.5, close, 100.0))
}

fn snap(pairs: &[(&str, Atom)]) -> Snapshot<String> {
    pairs
        .iter()
        .map(|(k, a)| ((*k).to_string(), a.clone()))
        .collect()
}

const T0: i64 = 1_710_506_096_000; // 2024-03-15 12:34:56 UTC — a Friday.

#[test]
fn pick_projects_the_named_asset() {
    let mut btc = Pick::<String>::new("BTC".into());
    let s = snap(&[("BTC", atom(100.0)), ("ETH", atom(50.0))]);
    let out = btc.update(s).expect("BTC present");
    assert_eq!(out.candle.close, 100.0);
}

#[test]
fn close_of_pick_reads_the_projected_close() {
    // `Close::of(Pick::new("BTC"))` = "BTC's close" as a Real indicator whose
    // Input is `Snapshot<String>`.
    let mut btc_close = Close::of(Pick::<String>::new("BTC".into()));
    let s = snap(&[("BTC", atom(101.5)), ("ETH", atom(4.25))]);
    assert_eq!(btc_close.update(s), Some(101.5));
}

#[test]
fn btc_eth_close_spread_composes_from_two_picks() {
    // The headline expression the refactor exists to support.
    let mut spread = Close::of(Pick::<String>::new("BTC".into()))
        .sub(Close::of(Pick::<String>::new("ETH".into())));
    let s = snap(&[("BTC", atom(100.0)), ("ETH", atom(60.0))]);
    assert_eq!(spread.update(s), Some(40.0));
}

#[test]
fn bar_indicator_stacks_on_a_pick() {
    // `Atr` takes `S: Indicator<Output = Candle>`; wrapping a Pick in
    // `CurrentBar::of(...)` re-projects Atom → Candle, so an ATR over BTC
    // reads the full bar out of the snapshot.
    let mut atr = Atr::new(CurrentBar::of(Pick::<String>::new("BTC".into())), 3);
    for i in 0..10 {
        let s = snap(&[
            ("BTC", atom(100.0 + i as Real)),
            ("ETH", atom(60.0 - i as Real)),
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
    let mut year = Year::of(Pick::<String>::new("BTC".into()));
    let s = snap(&[
        ("BTC", atom_at(T0, 100.0)),
        ("ETH", atom_at(T0 + 60_000, 50.0)),
    ]);
    assert_eq!(year.update(s), Some(2024.0));
}

#[test]
fn missing_asset_stays_none_downstream() {
    // BTC is not present — Close::of(Pick("BTC")) emits None, and the
    // downstream Sub is None too (both operands must be Some).
    let mut spread = Close::of(Pick::<String>::new("BTC".into()))
        .sub(Close::of(Pick::<String>::new("ETH".into())));
    let s = snap(&[("SOL", atom(20.0)), ("ETH", atom(10.0))]);
    assert_eq!(spread.update(s), None);
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
