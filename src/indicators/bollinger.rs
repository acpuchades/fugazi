use crate::indicator::Indicator;
use crate::indicators::Component;
use crate::indicators::stats::WindowStats;
use crate::types::Real;

/// The three bands of [`Bollinger`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BollingerValue {
    /// Upper band: `middle + k * stddev`.
    pub upper: Real,
    /// Middle band: the moving average.
    pub middle: Real,
    /// Lower band: `middle - k * stddev`.
    pub lower: Real,
}

/// Bollinger Bands of a source.
///
/// Owns its input source: `Bollinger::new(Current::close(), 20, 2.0)`. The
/// middle band is the simple moving average; the upper/lower bands sit `k`
/// (population) standard deviations away. A single shared [`WindowStats`] core
/// provides both the mean and the dispersion over the same window.
///
/// Bands are exposed as public fields and refreshed every update.
#[derive(Debug, Clone)]
pub struct Bollinger<S> {
    source: S,
    stats: WindowStats,
    k: Real,
    /// Latest upper band.
    pub upper: Option<Real>,
    /// Latest middle band (the moving average).
    pub middle: Option<Real>,
    /// Latest lower band.
    pub lower: Option<Real>,
}

impl<S> Bollinger<S> {
    /// `period` window, bands `k` standard deviations from the mean (typically
    /// `2.0`).
    ///
    /// # Panics
    /// Panics if `period` is zero.
    pub fn new(source: S, period: usize, k: Real) -> Self {
        Self {
            source,
            stats: WindowStats::new(period),
            k,
            upper: None,
            middle: None,
            lower: None,
        }
    }
}

/// Component accessors: each band as a standalone `Indicator<Output = Real>`, so
/// a band composes and compares like any other source — e.g.
/// `Current::close().crosses_above(bands.upper())`.
impl<S> Bollinger<S>
where
    Bollinger<S>: Indicator<Output = BollingerValue> + Clone,
{
    /// The upper band (`middle + k·stddev`) as a standalone source.
    pub fn upper(&self) -> Component<Self> {
        Component::new(self.clone(), |v| v.upper)
    }

    /// The middle band (the moving average) as a standalone source.
    pub fn middle(&self) -> Component<Self> {
        Component::new(self.clone(), |v| v.middle)
    }

    /// The lower band (`middle − k·stddev`) as a standalone source.
    pub fn lower(&self) -> Component<Self> {
        Component::new(self.clone(), |v| v.lower)
    }
}

impl<S: Indicator<Output = Real>> Indicator for Bollinger<S> {
    type Input = S::Input;
    type Output = BollingerValue;

    fn update(&mut self, input: Self::Input) -> Option<BollingerValue> {
        let ready = match self.source.update(input) {
            Some(x) => self.stats.update(x),
            None => false,
        };

        if ready {
            let middle = self.stats.mean();
            let band = self.k * self.stats.stddev();
            let upper = middle + band;
            let lower = middle - band;
            self.upper = Some(upper);
            self.middle = Some(middle);
            self.lower = Some(lower);
            Some(BollingerValue {
                upper,
                middle,
                lower,
            })
        } else {
            self.upper = None;
            self.middle = None;
            self.lower = None;
            None
        }
    }

    fn value(&self) -> Option<BollingerValue> {
        match (self.upper, self.middle, self.lower) {
            (Some(upper), Some(middle), Some(lower)) => Some(BollingerValue {
                upper,
                middle,
                lower,
            }),
            _ => None,
        }
    }

    fn warm_up_period(&self) -> usize {
        self.source.warm_up_period().max(1) + self.stats.period() - 1
    }

    fn unstable_period(&self) -> usize {
        self.source.unstable_period()
    }

    fn reset(&mut self) {
        self.source.reset();
        self.stats.reset();
        self.upper = None;
        self.middle = None;
        self.lower = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indicators::Identity;

    #[test]
    fn bands_straddle_the_mean() {
        let mut bb = Bollinger::new(Identity::new(), 3, 2.0);
        assert_eq!(bb.update(2.0), None);
        assert_eq!(bb.update(4.0), None);
        let out = bb.update(6.0).unwrap(); // mean 4, stddev sqrt(8/3)
        assert!((out.middle - 4.0).abs() < 1e-12);
        let sd = (8.0_f64 / 3.0).sqrt();
        assert!((out.upper - (4.0 + 2.0 * sd)).abs() < 1e-12);
        assert!((out.lower - (4.0 - 2.0 * sd)).abs() < 1e-12);
    }
}
