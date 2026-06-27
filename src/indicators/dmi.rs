use crate::indicator::Indicator;
use crate::indicators::Component;
use crate::indicators::smoothing::WilderState;
use crate::types::{Candle, Real};

/// The directional indicators of [`Dmi`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DmiValue {
    /// Positive directional indicator, `+DI`.
    pub plus_di: Real,
    /// Negative directional indicator, `-DI`.
    pub minus_di: Real,
}

/// Directional Movement Index (Wilder): the `+DI` / `-DI` pair.
///
/// A bar indicator (consumes the full [`Candle`]). Up-moves and down-moves are
/// reduced to `+DM` / `-DM`, each Wilder-smoothed alongside the true range; the
/// directional indicators are then `100·smoothed_DM / smoothed_TR`. This is the
/// directional core [`Adx`](super::Adx) builds on — `Adx` embeds a `Dmi` and
/// smooths the spread of these two lines into the trend-strength index.
///
/// The first bar only seeds the previous high/low/close, so `+DI` / `-DI` become
/// available after `period` further (directional) bars.
#[derive(Debug, Clone)]
pub struct Dmi {
    // Previous bar's high, low and close.
    prev: Option<(Real, Real, Real)>,
    plus_dm: WilderState,
    minus_dm: WilderState,
    true_range: WilderState,
    /// Latest `+DI`.
    pub plus_di: Option<Real>,
    /// Latest `-DI`.
    pub minus_di: Option<Real>,
}

impl Dmi {
    /// # Panics
    /// Panics if `period` is zero.
    pub fn new(period: usize) -> Self {
        Self {
            prev: None,
            plus_dm: WilderState::new(period),
            minus_dm: WilderState::new(period),
            true_range: WilderState::new(period),
            plus_di: None,
            minus_di: None,
        }
    }
}

/// Component accessors: each directional line as a standalone
/// `Indicator<Output = Real>`, so e.g.
/// `dmi.plus_di().crosses_above(dmi.minus_di())`.
impl Dmi {
    /// `+DI` as a standalone source.
    pub fn plus_di(&self) -> Component<Self> {
        Component::new(self.clone(), |v| v.plus_di)
    }

    /// `-DI` as a standalone source.
    pub fn minus_di(&self) -> Component<Self> {
        Component::new(self.clone(), |v| v.minus_di)
    }
}

impl Indicator for Dmi {
    type Input = Candle;
    type Output = DmiValue;

    fn update(&mut self, candle: Candle) -> Option<DmiValue> {
        let (prev_high, prev_low, prev_close) = match self.prev {
            Some(prev) => prev,
            None => {
                // First bar: no directional movement to measure yet.
                self.prev = Some((candle.high, candle.low, candle.close));
                return None;
            }
        };
        self.prev = Some((candle.high, candle.low, candle.close));

        let up_move = candle.high - prev_high;
        let down_move = prev_low - candle.low;
        let plus_dm = if up_move > down_move && up_move > 0.0 {
            up_move
        } else {
            0.0
        };
        let minus_dm = if down_move > up_move && down_move > 0.0 {
            down_move
        } else {
            0.0
        };
        let high_low = candle.high - candle.low;
        let high_close = (candle.high - prev_close).abs();
        let low_close = (candle.low - prev_close).abs();
        let tr = high_low.max(high_close).max(low_close);

        let smoothed_plus = self.plus_dm.update(plus_dm);
        let smoothed_minus = self.minus_dm.update(minus_dm);
        let smoothed_tr = self.true_range.update(tr);

        if let (Some(sp), Some(sm), Some(st)) = (smoothed_plus, smoothed_minus, smoothed_tr) {
            let (plus_di, minus_di) = if st == 0.0 {
                (0.0, 0.0)
            } else {
                (100.0 * sp / st, 100.0 * sm / st)
            };
            self.plus_di = Some(plus_di);
            self.minus_di = Some(minus_di);
        }

        self.value()
    }

    fn value(&self) -> Option<DmiValue> {
        match (self.plus_di, self.minus_di) {
            (Some(plus_di), Some(minus_di)) => Some(DmiValue { plus_di, minus_di }),
            _ => None,
        }
    }

    fn reset(&mut self) {
        self.prev = None;
        self.plus_dm.reset();
        self.minus_dm.reset();
        self.true_range.reset();
        self.plus_di = None;
        self.minus_di = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uptrend_has_plus_di_above_minus_di() {
        let mut dmi = Dmi::new(3);
        let mut last = None;
        for i in 0..8 {
            let base = 10.0 + i as Real;
            last = dmi.update(Candle::new(base, base + 1.0, base - 0.5, base + 0.5, 0.0));
        }
        let out = last.expect("dmi should be ready");
        assert!(out.plus_di > out.minus_di);
    }
}
