//! Candle field accessors: source indicators that extract one scalar from each
//! [`Candle`].
//!
//! All share the [`Field`] carrier, specialised by a [`CandleField`] marker, so
//! a new accessor is a trait impl rather than a new type. These make OHLCV data
//! composable with a *uniform* `Candle` input: define a whole signal in terms of
//! accessors and just feed each bar, e.g.
//! `Close::new().crosses_above(Close::new().then(Ema::new(20)))`.

use std::marker::PhantomData;

use crate::indicator::Indicator;
use crate::types::{Candle, Real};

/// Selects a scalar field from a [`Candle`].
pub trait CandleField {
    fn get(candle: &Candle) -> Real;
}

/// A source indicator that extracts one [`CandleField`] from each bar.
///
/// Use the aliases ([`Open`], [`High`], [`Low`], [`Close`], [`Volume`],
/// [`Typical`], [`Median`]).
#[derive(Debug, Clone)]
pub struct Field<F> {
    /// Latest extracted value; `None` before the first bar.
    pub value: Option<Real>,
    _field: PhantomData<fn() -> F>,
}

impl<F> Field<F> {
    pub fn new() -> Self {
        Self {
            value: None,
            _field: PhantomData,
        }
    }
}

impl<F> Default for Field<F> {
    fn default() -> Self {
        Self::new()
    }
}

impl<F: CandleField> Indicator for Field<F> {
    type Input = Candle;
    type Output = Real;

    fn update(&mut self, candle: Candle) -> Option<Real> {
        self.value = Some(F::get(&candle));
        self.value
    }

    fn value(&self) -> Option<Real> {
        self.value
    }

    fn reset(&mut self) {
        self.value = None;
    }
}

/// The `open` price.
#[derive(Debug, Clone, Copy)]
pub struct OpenField;
impl CandleField for OpenField {
    fn get(candle: &Candle) -> Real {
        candle.open
    }
}

/// The `high` price.
#[derive(Debug, Clone, Copy)]
pub struct HighField;
impl CandleField for HighField {
    fn get(candle: &Candle) -> Real {
        candle.high
    }
}

/// The `low` price.
#[derive(Debug, Clone, Copy)]
pub struct LowField;
impl CandleField for LowField {
    fn get(candle: &Candle) -> Real {
        candle.low
    }
}

/// The `close` price.
#[derive(Debug, Clone, Copy)]
pub struct CloseField;
impl CandleField for CloseField {
    fn get(candle: &Candle) -> Real {
        candle.close
    }
}

/// The `volume`.
#[derive(Debug, Clone, Copy)]
pub struct VolumeField;
impl CandleField for VolumeField {
    fn get(candle: &Candle) -> Real {
        candle.volume
    }
}

/// The typical price, `(high + low + close) / 3`.
#[derive(Debug, Clone, Copy)]
pub struct TypicalField;
impl CandleField for TypicalField {
    fn get(candle: &Candle) -> Real {
        candle.typical()
    }
}

/// The median price, `(high + low) / 2`.
#[derive(Debug, Clone, Copy)]
pub struct MedianField;
impl CandleField for MedianField {
    fn get(candle: &Candle) -> Real {
        candle.median()
    }
}

/// The bar's `open`.
pub type Open = Field<OpenField>;
/// The bar's `high`.
pub type High = Field<HighField>;
/// The bar's `low`.
pub type Low = Field<LowField>;
/// The bar's `close`.
pub type Close = Field<CloseField>;
/// The bar's `volume`.
pub type Volume = Field<VolumeField>;
/// The bar's typical price, `(high + low + close) / 3`.
pub type Typical = Field<TypicalField>;
/// The bar's median price, `(high + low) / 2`.
pub type Median = Field<MedianField>;

/// Namespace for building [`Candle`]-input accessor sources.
///
/// Reads as "the current bar's `<field>`":
/// `Current::close().crosses_above(Current::close().then(Ema::new(20)))`.
/// Each method just constructs the corresponding [`Field`] alias.
#[derive(Debug, Clone, Copy)]
pub struct Current;

impl Current {
    /// The current bar's open.
    pub fn open() -> Open {
        Open::new()
    }

    /// The current bar's high.
    pub fn high() -> High {
        High::new()
    }

    /// The current bar's low.
    pub fn low() -> Low {
        Low::new()
    }

    /// The current bar's close.
    pub fn close() -> Close {
        Close::new()
    }

    /// The current bar's volume.
    pub fn volume() -> Volume {
        Volume::new()
    }

    /// The current bar's typical price, `(high + low + close) / 3`.
    pub fn typical() -> Typical {
        Typical::new()
    }

    /// The current bar's median price, `(high + low) / 2`.
    pub fn median() -> Median {
        Median::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accessors_extract_fields() {
        let bar = Candle::new(1.0, 4.0, 0.5, 3.0, 1000.0);
        assert_eq!(Current::close().update(bar), Some(3.0));
        assert_eq!(Current::volume().update(bar), Some(1000.0));
        assert_eq!(Current::high().update(bar), Some(4.0));
    }
}
