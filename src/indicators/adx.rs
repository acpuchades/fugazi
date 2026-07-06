use crate::indicator::Indicator;
use crate::indicators::smoothing::WilderState;
use crate::indicators::{Component, Dmi};
use crate::types::{Candle, Real};

/// The directional outputs of [`Adx`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AdxValue {
    /// Positive directional indicator, `+DI`.
    pub plus_di: Real,
    /// Negative directional indicator, `-DI`.
    pub minus_di: Real,
    /// The average directional index, `ADX`.
    pub adx: Real,
}

/// Average Directional Index (Wilder).
///
/// A bar indicator (consumes candles from an owned source). Built on a [`Dmi`]
/// core: the `+DI` / `-DI` pair come straight from it, and their normalised
/// spread `DX = 100·|+DI − −DI|/(+DI + −DI)` is Wilder-smoothed again to
/// produce `ADX`.
///
/// `+DI` and `-DI` become available after `period` directional bars; `adx`
/// follows after a further `period` bars. The directional fields are exposed
/// individually; [`value`](Indicator::value) / [`update`](Indicator::update)
/// only yield a value once `adx` itself is ready.
#[derive(Debug, Clone)]
pub struct Adx<S> {
    dmi: Dmi<S>,
    dx: WilderState,
    /// Latest `+DI`.
    pub plus_di: Option<Real>,
    /// Latest `-DI`.
    pub minus_di: Option<Real>,
    /// Latest `ADX`.
    pub adx: Option<Real>,
}

impl<S> Adx<S> {
    /// Create a new ADX over `source` and `period`.
    ///
    /// # Panics
    /// Panics if `period` is zero.
    pub fn new(source: S, period: usize) -> Self {
        Self {
            dmi: Dmi::new(source, period),
            dx: WilderState::new(period),
            plus_di: None,
            minus_di: None,
            adx: None,
        }
    }
}

/// Component accessors: each output as a standalone `Indicator<Output = Real>`,
/// so e.g. a trend filter reads `adx.adx().above(25.0)` or
/// `adx.plus_di().crosses_above(adx.minus_di())`.
impl<S: Clone> Adx<S>
where
    Adx<S>: Indicator<Output = AdxValue>,
{
    /// `+DI` as a standalone source.
    pub fn plus_di(&self) -> Component<Self> {
        Component::new(self.clone(), |v| v.plus_di)
    }

    /// `-DI` as a standalone source.
    pub fn minus_di(&self) -> Component<Self> {
        Component::new(self.clone(), |v| v.minus_di)
    }

    /// `ADX` (trend strength) as a standalone source.
    pub fn adx(&self) -> Component<Self> {
        Component::new(self.clone(), |v| v.adx)
    }
}

impl<S: Indicator<Output = Candle>> Indicator for Adx<S> {
    type Input = S::Input;
    type Output = AdxValue;

    fn update(&mut self, input: S::Input) -> Option<AdxValue> {
        if let Some(di) = self.dmi.update(input) {
            self.plus_di = Some(di.plus_di);
            self.minus_di = Some(di.minus_di);

            let sum = di.plus_di + di.minus_di;
            let dx = if sum == 0.0 {
                0.0
            } else {
                100.0 * (di.plus_di - di.minus_di).abs() / sum
            };
            self.adx = self.dx.update(dx);
        }

        self.value()
    }

    fn value(&self) -> Option<AdxValue> {
        match (self.plus_di, self.minus_di, self.adx) {
            (Some(plus_di), Some(minus_di), Some(adx)) => Some(AdxValue {
                plus_di,
                minus_di,
                adx,
            }),
            _ => None,
        }
    }

    fn warm_up_period(&self) -> usize {
        // DX values start with the DMI; the second Wilder pass then consumes a
        // full period of them before `adx` is ready.
        self.dmi.warm_up_period() + self.dx.period() - 1
    }

    fn unstable_period(&self) -> usize {
        // The DI lines must settle, then the DX smoothing settles on top.
        self.dmi.unstable_period() + self.dx.settle_period()
    }

    fn reset(&mut self) {
        self.dmi.reset();
        self.dx.reset();
        self.plus_di = None;
        self.minus_di = None;
        self.adx = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indicators::Current;
    use crate::types::Candle;

    #[test]
    fn strong_uptrend_has_plus_di_above_minus_di() {
        let mut adx = Adx::new(Current::candle(), 3);
        let mut last = None;
        // Steadily rising bars: +DI should dominate -DI.
        for i in 0..12 {
            let base = 10.0 + i as Real;
            last = adx.update(Candle::new(base, base + 1.0, base - 0.5, base + 0.5, 0.0).into());
        }
        let out = last.expect("adx should be ready");
        assert!(out.plus_di > out.minus_di);
        assert!((0.0..=100.0).contains(&out.adx));
    }
}
