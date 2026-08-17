//! Synthetic bars, atoms and snapshot streams.
//!
//! Only the *shapes* live here — the ones six test crates each had their own
//! copy of. Series **constants** deliberately do not: a file whose assertions
//! depend on exactly which crossovers its price path fires (`resume.rs`,
//! `montecarlo.rs`, `strategies.rs`) owns its own generator, because sharing
//! one would couple those expectations together and a tweak for one test would
//! silently retune the others.

use fugazi::prelude::*;
use fugazi::types::{Snapshot, Symbol, symbol as intern};

/// One day in milliseconds — the cadence [`daily_series`] stamps.
pub const DAY_MS: i64 = 86_400_000;

/// A flat bar: `open == high == low == close`, unit volume.
///
/// The workhorse for wallet arithmetic. `open == close` means a market order
/// filling at the next bar's open fills at that bar's close, so every expected
/// fill price is checkable by eye, and a zero range means no protective leg can
/// trigger intrabar.
pub fn flat(px: Real) -> Candle {
    Candle::new(px, px, px, px, 1.0)
}

/// [`flat`] with an explicit volume, for the volume/flow indicators (`Obv`,
/// `Ad`, `Mfi`, `Vwap`) that read `0.0` as a degenerate bar.
pub fn flat_with_volume(px: Real, volume: Real) -> Candle {
    Candle::new(px, px, px, px, volume)
}

/// A bar with a symmetric `±1` intrabar range around `px` and a round volume.
///
/// The shape to reach for when a test needs `high`/`low` to actually differ
/// from the close — true-range and protective-trigger paths read them — but
/// doesn't care about the exact range.
pub fn banded(px: Real) -> Candle {
    Candle::new(px, px + 1.0, px - 1.0, px, 1_000.0)
}

/// A price-less atom: an overlay series carries values but no candle, so
/// `backtest::run` skips it for wallet pricing while the strategy still sees it.
pub fn overlay_only_atom() -> Atom {
    Atom::overlay_only(
        OverlayInfo::new(Schema::empty(), Vec::new()),
        Timestamp(0),
    )
}

/// One untimed single-symbol snapshot per close, built from `shape`.
///
/// `shape` is the per-bar candle builder — pass [`flat`] when fill prices must
/// be readable by eye, [`banded`] when the bar needs a real range.
pub fn series(
    symbol: &str,
    closes: &[Real],
    shape: fn(Real) -> Candle,
) -> Vec<Snapshot<Symbol>> {
    // Interned once for the whole series; each bar's tag is a refcount bump.
    let sym = intern(symbol);
    closes
        .iter()
        .map(|&px| Snapshot::single(sym.clone(), Atom::new(shape(px))))
        .collect()
}

/// One snapshot per bar carrying every column, all stamped on a daily cadence
/// from the epoch — the aligned cross-sectional stream.
///
/// # Panics
///
/// If the columns are ragged. Every symbol must quote on every bar; a test that
/// wants a listing gap should build the stream by hand, because *which* bar is
/// missing is the thing it is asserting.
pub fn daily_series(
    columns: &[(&str, &[Real])],
    shape: fn(Real) -> Candle,
) -> Vec<Snapshot<Symbol>> {
    let bars = columns.first().map_or(0, |(_, c)| c.len());
    assert!(
        columns.iter().all(|(_, c)| c.len() == bars),
        "ragged columns: {:?}",
        columns.iter().map(|(s, c)| (*s, c.len())).collect::<Vec<_>>()
    );
    (0..bars)
        .map(|i| {
            let t = Timestamp(i as i64 * DAY_MS);
            let mut snap = Snapshot::<Symbol>::new();
            for (symbol, closes) in columns {
                snap.push(
                    Some(intern(*symbol)),
                    None,
                    Atom::with_time(shape(closes[i]), t),
                );
            }
            snap
        })
        .collect()
}

/// Assert two floats agree to `tol` **relative** to the larger magnitude,
/// falling back to absolute near zero. Reports both sides on failure.
#[track_caller]
pub fn assert_close(got: Real, want: Real, tol: Real, what: &str) {
    let scale = got.abs().max(want.abs()).max(1.0);
    assert!(
        (got - want).abs() <= tol * scale,
        "{what}: got {got}, want {want} (tol {tol} relative, diff {})",
        (got - want).abs()
    );
}
