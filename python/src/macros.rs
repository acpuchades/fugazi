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
            AnySource::Candle($s) => AnySource::Candle(Source::new($build)),
            AnySource::Real($s) => AnySource::Real(Source::new($build)),
            AnySource::Snapshot($s) => AnySource::Snapshot(Source::new($build)),
            AnySource::Const(c) => {
                let $s = const_to_candle_source(c);
                AnySource::Candle(Source::new($build))
            }
        }
    };
}

/// Combine two sources into a new source; resolves a constant against its
/// partner, errors on a genuine domain clash.
macro_rules! combine_sources {
    ($lhs:expr, $rhs:expr, |$l:ident, $r:ident| $build:expr) => {
        pair($lhs, $rhs).map(|p| match p {
            Pair::Candle($l, $r) => AnySource::Candle(Source::new($build)),
            Pair::Real($l, $r) => AnySource::Real(Source::new($build)),
            Pair::Snapshot($l, $r) => AnySource::Snapshot(Source::new($build)),
        })
    };
}

/// Turn one source into a signal, preserving its domain. A neutral constant
/// defaults to the candle domain.
macro_rules! source_to_signal {
    ($src:expr, |$s:ident| $build:expr) => {
        match $src {
            AnySource::Candle($s) => AnySignal::Candle(SignalBox::new($build)),
            AnySource::Real($s) => AnySignal::Real(SignalBox::new($build)),
            AnySource::Snapshot($s) => AnySignal::Snapshot(SignalBox::new($build)),
            AnySource::Const(c) => {
                let $s = const_to_candle_source(c);
                AnySignal::Candle(SignalBox::new($build))
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
            AnySource::Candle($s) => AnyMulti::Candle(MultiBox::new($build)),
            AnySource::Real($s) => AnyMulti::Real(MultiBox::new($build)),
            AnySource::Snapshot($s) => AnyMulti::Snapshot(MultiBox::new($build)),
            AnySource::Const(c) => {
                let $s = const_to_candle_source(c);
                AnyMulti::Candle(MultiBox::new($build))
            }
        }
    };
}

/// Wrap two sources in a multi-output constructor; resolves a constant against
/// its partner, errors on a genuine domain clash.
macro_rules! combine_multi {
    ($lhs:expr, $rhs:expr, |$l:ident, $r:ident| $build:expr) => {
        pair($lhs, $rhs).map(|p| match p {
            Pair::Candle($l, $r) => AnyMulti::Candle(MultiBox::new($build)),
            Pair::Real($l, $r) => AnyMulti::Real(MultiBox::new($build)),
            Pair::Snapshot($l, $r) => AnyMulti::Snapshot(MultiBox::new($build)),
        })
    };
}

