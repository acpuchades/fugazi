//! Domain-preserving dispatch macros.
//!
//! Hoisted into their own module so `#[macro_use]` on the `mod`
//! declaration makes them visible to every module below it in
//! `lib.rs`, which is where they were all used from anyway.

#![allow(unused_macros)]

/// Apply a source-wrapping constructor to a source, preserving its domain. A
/// neutral constant defaults to the candle domain.
macro_rules! map_source {
    ($src:expr, |$s:ident| $build:expr) => {
        match $src {
            AnySource::Candle($s) => AnySource::Candle(runtime::erase($build)),
            AnySource::Atom($s) => AnySource::Atom(runtime::erase($build)),
            AnySource::Real($s) => AnySource::Real(runtime::erase($build)),
            AnySource::Snapshot($s) => AnySource::Snapshot(runtime::erase($build)),
            AnySource::Const(c) => {
                let $s = const_to_atom_source(c);
                AnySource::Atom(runtime::erase($build))
            }
        }
    };
}

/// Apply a source-wrapping constructor, **fusing a plain root** when there is
/// one.
///
/// The fusing twin of [`map_source!`], and the only place roots are observed.
/// Takes the whole `PyIndicator` rather than its `src`, because the root lives
/// beside `src` on the carrier — see [`PendingRoot`] for why it lives there and
/// what it is worth.
///
/// The two extra arms monomorphise `$build` over the concrete root, so each
/// wrapping constructor gains two instantiations. That is the price of fusing,
/// and it is why the bar root is one `BarFieldDyn` with a runtime field rather
/// than seven typed markers: seven would have multiplied, two only add.
///
/// The `None` arm is exactly `map_source!`, so an unrooted chain behaves as it
/// always did.
macro_rules! map_rooted {
    ($ind:expr, |$s:ident| $build:expr) => {{
        let ind = $ind;
        match ind.root.clone() {
            Some(PendingRoot::Real($s)) => AnySource::Real(runtime::erase($build)),
            // Seven arms, one per field, so `$build` monomorphises over a typed
            // `BarField<F>` and the fused chain does a direct load. A runtime
            // field enum would collapse these into one instantiation and cost ~5
            // instructions/sample doing it — measured, see `BarField`.
            Some(PendingRoot::Field(k)) => {
                macro_rules! fuse_field {
                    ($marker:ty) => {{
                        let $s = BarField::<$marker>::new();
                        AnySource::Candle(runtime::erase($build))
                    }};
                }
                match k {
                    BarFieldKind::Open => fuse_field!(BarOpen),
                    BarFieldKind::High => fuse_field!(BarHigh),
                    BarFieldKind::Low => fuse_field!(BarLow),
                    BarFieldKind::Close => fuse_field!(BarClose),
                    BarFieldKind::Volume => fuse_field!(BarVolume),
                    BarFieldKind::Typical => fuse_field!(BarTypical),
                    BarFieldKind::Median => fuse_field!(BarMedian),
                }
            }
            None => map_source!(ind.src.clone(), |$s| $build),
        }
    }};
}

/// Combine two sources into a new source; resolves a constant against its
/// partner, errors on a genuine domain clash.
macro_rules! combine_sources {
    ($lhs:expr, $rhs:expr, |$l:ident, $r:ident| $build:expr) => {
        pair($lhs, $rhs).map(|p| match p {
            Pair::Candle($l, $r) => AnySource::Candle(runtime::erase($build)),
            Pair::Atom($l, $r) => AnySource::Atom(runtime::erase($build)),
            Pair::Real($l, $r) => AnySource::Real(runtime::erase($build)),
            Pair::Snapshot($l, $r) => AnySource::Snapshot(runtime::erase($build)),
        })
    };
}

/// Turn one source into a signal, preserving its domain. A neutral constant
/// defaults to the candle domain.
macro_rules! source_to_signal {
    ($src:expr, |$s:ident| $build:expr) => {
        match $src {
            AnySource::Candle($s) => AnySignal::Candle(SignalBox::new($build)),
            AnySource::Atom($s) => AnySignal::Atom(SignalBox::new($build)),
            AnySource::Real($s) => AnySignal::Real(SignalBox::new($build)),
            AnySource::Snapshot($s) => AnySignal::Snapshot(SignalBox::new($build)),
            AnySource::Const(c) => {
                let $s = const_to_atom_source(c);
                AnySignal::Atom(SignalBox::new($build))
            }
        }
    };
}

/// Turn two sources into a signal; resolves a constant against its partner,
/// errors on a genuine domain clash.
macro_rules! sources_to_signal {
    ($lhs:expr, $rhs:expr, |$l:ident, $r:ident| $build:expr) => {
        pair($lhs, $rhs).map(|p| match p {
            Pair::Candle($l, $r) => AnySignal::Candle(SignalBox::new($build)),
            Pair::Atom($l, $r) => AnySignal::Atom(SignalBox::new($build)),
            Pair::Real($l, $r) => AnySignal::Real(SignalBox::new($build)),
            Pair::Snapshot($l, $r) => AnySignal::Snapshot(SignalBox::new($build)),
        })
    };
}

/// Transform one signal, preserving its domain.
macro_rules! map_signal {
    ($sig:expr, |$s:ident| $build:expr) => {
        match $sig {
            AnySignal::Candle($s) => AnySignal::Candle(SignalBox::new($build)),
            AnySignal::Atom($s) => AnySignal::Atom(SignalBox::new($build)),
            AnySignal::Real($s) => AnySignal::Real(SignalBox::new($build)),
            AnySignal::Snapshot($s) => AnySignal::Snapshot(SignalBox::new($build)),
        }
    };
}

/// Combine two signals; errors if their domains differ.
macro_rules! combine_signals {
    ($lhs:expr, $rhs:expr, |$l:ident, $r:ident| $build:expr) => {
        match ($lhs, $rhs) {
            (AnySignal::Candle($l), AnySignal::Candle($r)) => {
                Ok(AnySignal::Candle(SignalBox::new($build)))
            }
            (AnySignal::Atom($l), AnySignal::Atom($r)) => {
                Ok(AnySignal::Atom(SignalBox::new($build)))
            }
            // Mixed bar/atom: lift the bar side. Rejecting would break
            // `close().above(1).and_(get(schema,"f").above(0))`, which was valid
            // when the two were one domain.
            (AnySignal::Candle($l), AnySignal::Atom($r)) => {
                let $l = atom_signal_over_candle($l);
                Ok(AnySignal::Atom(SignalBox::new($build)))
            }
            (AnySignal::Atom($l), AnySignal::Candle($r)) => {
                let $r = atom_signal_over_candle($r);
                Ok(AnySignal::Atom(SignalBox::new($build)))
            }
            (AnySignal::Real($l), AnySignal::Real($r)) => {
                Ok(AnySignal::Real(SignalBox::new($build)))
            }
            (AnySignal::Snapshot($l), AnySignal::Snapshot($r)) => {
                Ok(AnySignal::Snapshot(SignalBox::new($build)))
            }
            _ => Err(domain_mismatch()),
        }
    };
}

/// Wrap one source in a multi-output constructor, preserving its domain. A
/// neutral constant defaults to the candle domain.
macro_rules! map_multi {
    ($src:expr, |$s:ident| $build:expr) => {
        match $src {
            AnySource::Candle($s) => {
                let $s = atom_over_candle($s);
                AnyMulti::Atom(MultiBox::new($build))
            }
            AnySource::Atom($s) => AnyMulti::Atom(MultiBox::new($build)),
            AnySource::Real($s) => AnyMulti::Real(MultiBox::new($build)),
            AnySource::Snapshot($s) => AnyMulti::Snapshot(MultiBox::new($build)),
            AnySource::Const(c) => {
                let $s = const_to_atom_source(c);
                AnyMulti::Atom(MultiBox::new($build))
            }
        }
    };
}

/// Wrap two sources in a multi-output constructor; resolves a constant against
/// its partner, errors on a genuine domain clash.
macro_rules! combine_multi {
    ($lhs:expr, $rhs:expr, |$l:ident, $r:ident| $build:expr) => {
        pair($lhs, $rhs).map(|p| match p {
            // `AnyMulti` has no bar-only domain yet (step 3 of the plan), so a
            // bar-only pair is lifted rather than left unbuildable.
            Pair::Candle($l, $r) => {
                let ($l, $r) = (atom_over_candle($l), atom_over_candle($r));
                AnyMulti::Atom(MultiBox::new($build))
            }
            Pair::Atom($l, $r) => AnyMulti::Atom(MultiBox::new($build)),
            Pair::Real($l, $r) => AnyMulti::Real(MultiBox::new($build)),
            Pair::Snapshot($l, $r) => AnyMulti::Snapshot(MultiBox::new($build)),
        })
    };
}

