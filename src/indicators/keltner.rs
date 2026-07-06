use crate::indicator::Indicator;
use crate::indicators::{Atr, Ema};
use crate::types::{Candle, Real};

/// The three lines of a [`Keltner`] channel.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KeltnerValue {
    /// Upper channel: `middle + multiplier · ATR`.
    pub upper: Real,
    /// Middle channel: the EMA of the source.
    pub middle: Real,
    /// Lower channel: `middle − multiplier · ATR`.
    pub lower: Real,
}

/// Keltner Channels around an EMA, banded by the Average True Range.
///
/// Pure composition of existing pieces: the middle line is an [`Ema`] of a
/// price source, and the bands sit `multiplier` [`Atr`]s above and below it,
/// with the ATR reading its own candle source. The two sources must share an
/// `Input` — the classic channel around the close from the base bar stream is
/// `Keltner::new(Current::close(), Current::candle(), 20, 10, 2.0)`. Ready once
/// the ATR has warmed up (after `atr_period` bars); the EMA seeds on the first
/// bar.
#[derive(Debug, Clone)]
pub struct Keltner<P, C> {
    ema: Ema<P>,
    atr: Atr<C>,
    multiplier: Real,
    /// Latest upper channel.
    pub upper: Option<Real>,
    /// Latest middle channel (the EMA).
    pub middle: Option<Real>,
    /// Latest lower channel.
    pub lower: Option<Real>,
}

impl<P, C> Keltner<P, C> {
    /// EMA of `price_source` over `ema_period` as the middle line, banded by
    /// `multiplier` times the [`Atr`] of `candle_source` over `atr_period`.
    ///
    /// # Panics
    /// Panics if either period is zero.
    pub fn new(
        price_source: P,
        candle_source: C,
        ema_period: usize,
        atr_period: usize,
        multiplier: Real,
    ) -> Self {
        Self {
            ema: Ema::new(price_source, ema_period),
            atr: Atr::new(candle_source, atr_period),
            multiplier,
            upper: None,
            middle: None,
            lower: None,
        }
    }
}

// Component accessors: each channel line as a standalone
// `Indicator<Output = Real>`, so a line composes and compares like any other
// source — e.g. `Current::close().crosses_above(channel.upper())`.
crate::indicators::component::component_accessors!(
    Keltner<P, C>, KeltnerValue;
    /// The upper channel (`middle + multiplier·ATR`) as a standalone source.
    upper => upper,
    /// The middle channel (the EMA) as a standalone source.
    middle => middle,
    /// The lower channel (`middle − multiplier·ATR`) as a standalone source.
    lower => lower,
);

impl<P, C> Indicator for Keltner<P, C>
where
    P: Indicator<Output = Real>,
    C: Indicator<Input = P::Input, Output = Candle>,
    P::Input: Clone,
{
    type Input = P::Input;
    type Output = KeltnerValue;

    fn update(&mut self, input: Self::Input) -> Option<KeltnerValue> {
        let middle = self.ema.update(input.clone());
        let atr = self.atr.update(input);

        match (middle, atr) {
            (Some(middle), Some(atr)) => {
                let band = self.multiplier * atr;
                let upper = middle + band;
                let lower = middle - band;
                self.upper = Some(upper);
                self.middle = Some(middle);
                self.lower = Some(lower);
                Some(KeltnerValue {
                    upper,
                    middle,
                    lower,
                })
            }
            _ => {
                self.upper = None;
                self.middle = None;
                self.lower = None;
                None
            }
        }
    }

    fn value(&self) -> Option<KeltnerValue> {
        match (self.upper, self.middle, self.lower) {
            (Some(upper), Some(middle), Some(lower)) => Some(KeltnerValue {
                upper,
                middle,
                lower,
            }),
            _ => None,
        }
    }

    fn warm_up_period(&self) -> usize {
        self.ema.warm_up_period().max(self.atr.warm_up_period())
    }

    fn unstable_period(&self) -> usize {
        self.ema.stable_period().max(self.atr.stable_period()) - self.warm_up_period()
    }

    fn reset(&mut self) {
        self.ema.reset();
        self.atr.reset();
        self.upper = None;
        self.middle = None;
        self.lower = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indicators::Current;
    use crate::types::Candle;

    fn bar(high: Real, low: Real, close: Real) -> Candle {
        Candle::new(low, high, low, close, 0.0)
    }

    #[test]
    fn bands_straddle_the_ema() {
        let mut kc = Keltner::new(Current::close(), Current::candle(), 3, 3, 2.0);
        assert_eq!(kc.update(bar(10.0, 9.0, 9.5).into()), None);
        assert_eq!(kc.update(bar(11.0, 10.0, 10.5).into()), None);
        let out = kc.update(bar(12.0, 11.0, 11.5).into()).unwrap();
        assert!(out.upper > out.middle);
        assert!(out.middle > out.lower);
        // Symmetric around the middle.
        assert!(((out.upper - out.middle) - (out.middle - out.lower)).abs() < 1e-12);
    }
}
