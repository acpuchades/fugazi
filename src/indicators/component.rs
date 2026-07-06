//! Projecting one `Real` component out of a multi-output indicator.

use crate::indicator::Indicator;
use crate::types::Real;

/// Adapts a multi-output indicator into a single-output [`Indicator`] that
/// yields just one of its component fields.
///
/// Multi-output indicators ([`Macd`](super::Macd), [`Bollinger`](super::Bollinger),
/// …) set their [`Output`](Indicator::Output) to a small struct, which means a
/// component like the MACD signal line or a Bollinger band cannot, on its own,
/// feed the [`Real`]-only composition and comparison machinery (`gt`, `add`,
/// `crosses_above`, …). `Component` closes that gap: it wraps the source and a
/// field selector and presents the chosen field as an ordinary
/// `Indicator<Output = Real>`, so it composes like any other source:
///
/// ```
/// use fugazi::prelude::*;
/// use fugazi::indicators::{Current, Macd};
///
/// let macd = Macd::new(Current::close(), 12, 26, 9);
/// // The MACD line crossing above its signal line — a single composed Signal.
/// let _cross = macd.line().crosses_above(macd.signal());
/// ```
///
/// You build one through the component accessors on each multi-output indicator
/// (`macd.line()`, `bands.upper()`, `adx.plus_di()`, …) rather than naming it
/// directly. Each accessor **clones** the source and pairs it with the selector,
/// so two components of the same indicator are two independently-advanced
/// instances — the same clone-the-operands tradeoff [`crosses_above`] already
/// makes (correct, at roughly the source work per component).
///
/// [`crosses_above`]: crate::indicators::IndicatorExt::crosses_above
#[derive(Debug, Clone)]
pub struct Component<I: Indicator> {
    source: I,
    select: fn(I::Output) -> Real,
    /// Latest projected component; `None` until the source is warmed up.
    pub value: Option<Real>,
}

impl<I: Indicator> Component<I> {
    /// Wrap `source`, projecting the field picked out by `select`.
    ///
    /// Prefer the named accessors on the indicators themselves (`macd.line()`,
    /// …); this is the underlying constructor they call.
    pub fn new(source: I, select: fn(I::Output) -> Real) -> Self {
        Self {
            source,
            select,
            value: None,
        }
    }
}

impl<I: Indicator> Indicator for Component<I> {
    type Input = I::Input;
    type Output = Real;

    fn update(&mut self, input: Self::Input) -> Option<Real> {
        self.value = self.source.update(input).map(self.select);
        self.value
    }

    fn value(&self) -> Option<Real> {
        self.value
    }

    fn warm_up_period(&self) -> usize {
        // `max(1)` guards a `warm_up = 0` inner (e.g. `Value`) — projection
        // still needs one `update` to advance the source.
        self.source.warm_up_period().max(1)
    }

    fn unstable_period(&self) -> usize {
        self.source.unstable_period()
    }

    fn reset(&mut self) {
        self.source.reset();
        self.value = None;
    }
}

#[cfg(test)]
mod tests {
    use crate::indicator::Indicator;
    use crate::indicators::{Bollinger, Current, Macd};
    use crate::indicators::{BoolIndicatorExt, IndicatorExt};
    use crate::types::{Candle, Real};

    fn bar(close: Real) -> Candle {
        Candle::new(close, close, close, close, 0.0)
    }

    #[test]
    fn projects_the_same_value_as_the_indicator_field() {
        let mut line = Macd::new(Current::close(), 3, 6, 4).line();
        let mut reference = Macd::new(Current::close(), 3, 6, 4);
        for p in [10.0, 11.0, 12.0, 11.5, 13.0, 14.0, 13.5, 15.0, 16.0, 15.0] {
            let projected = line.update(bar(p).into());
            let whole = reference.update(bar(p).into());
            assert_eq!(projected, whole.map(|v| v.macd));
        }
    }

    #[test]
    fn components_compose_into_a_crossover() {
        let macd = Macd::new(Current::close(), 3, 6, 4);
        // Exactly the user's target: line crosses above signal, as one Signal.
        let mut bullish = macd.line().crosses_above(macd.signal());
        let mut fired = false;
        // A dip then a sustained rally drives the MACD line up through its signal.
        for p in [
            20.0, 19.0, 18.0, 17.0, 18.0, 20.0, 22.0, 24.0, 26.0, 28.0, 30.0, 32.0,
        ] {
            bullish.update(bar(p).into());
            fired |= bullish.is_true();
        }
        assert!(fired, "expected a bullish MACD crossover");
    }

    #[test]
    fn band_component_projects_the_band_field() {
        // The lower band, projected as a source, matches the indicator's field —
        // so `Current::close().lt(bands.lower())` means exactly "close below the
        // lower band".
        let mut lower = Bollinger::new(Current::close(), 5, 2.0).lower();
        let mut reference = Bollinger::new(Current::close(), 5, 2.0);
        for p in [10.0, 10.1, 9.9, 10.0, 10.05, 18.0, 12.0, 11.0, 9.0, 8.5] {
            assert_eq!(
                lower.update(bar(p).into()),
                reference.update(bar(p).into()).map(|v| v.lower)
            );
        }
    }
}
