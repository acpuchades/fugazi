//! Cross-asset projection: pick one asset's [`Atom`] out of a
//! [`Snapshot`] by key.
//!
//! The [`Pick`] leaf turns a multi-asset [`Snapshot`] input into a single-asset
//! [`Atom`] output, so every existing source-generic candle-input leaf
//! ([`Close`](super::Close), [`Atr`](super::Atr), [`Year`](super::Year), …)
//! composes on top of it without any type gymnastics.
//!
//! ```ignore
//! use fugazi::indicators::{Close, Pick};
//! use fugazi::prelude::*;
//! // BTC/ETH close spread as a plain `Real`-output indicator over Snapshot.
//! let spread = Close::of(Pick::new("BTC"))
//!     .sub(Close::of(Pick::new("ETH")));
//! ```

use std::hash::Hash;

use crate::indicator::Indicator;
use crate::indicators::Identity;
use crate::types::{Atom, Snapshot};

/// Projects one asset's [`Atom`] out of a [`Snapshot`] by key.
///
/// `Input = S::Input`, `Output = Atom`. The default source `Identity<Snapshot<K>>`
/// makes `Pick::new(key)` a leaf that consumes a [`Snapshot`] directly;
/// `Pick::of(key, source)` re-roots it onto any indicator that emits a
/// `Snapshot<K>` (a resampler, a latch, an outer pick chain, …).
///
/// Emits `None` on bars where the key isn't present in the snapshot — the same
/// `None`-until-warm convention every other leaf uses, so a downstream
/// comparison stays `None` until the projected asset first appears.
#[derive(Debug, Clone)]
pub struct Pick<K, S = Identity<Snapshot<K>>> {
    key: K,
    source: S,
    /// The last atom projected out; `None` before the first bar or if the last
    /// snapshot lacked the key.
    pub value: Option<Atom>,
}

impl<K> Pick<K, Identity<Snapshot<K>>> {
    /// A [`Pick`] rooted on the raw [`Snapshot`] input stream.
    pub fn new(key: K) -> Self {
        Self::of(key, Identity::new())
    }
}

impl<K, S> Pick<K, S> {
    /// A [`Pick`] rooted on a custom snapshot-emitting source.
    pub fn of(key: K, source: S) -> Self {
        Self {
            key,
            source,
            value: None,
        }
    }

    /// The key this pick reads out of every snapshot.
    pub fn key(&self) -> &K {
        &self.key
    }
}

impl<K, S> Indicator for Pick<K, S>
where
    K: Clone + Eq + Hash,
    S: Indicator<Output = Snapshot<K>>,
{
    type Input = S::Input;
    type Output = Atom;

    fn update(&mut self, input: S::Input) -> Option<Atom> {
        self.value = self
            .source
            .update(input)
            .and_then(|s| s.get(&self.key).cloned());
        self.value.clone()
    }

    fn value(&self) -> Option<Atom> {
        self.value.clone()
    }

    fn warm_up_period(&self) -> usize {
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
    use super::*;
    use crate::types::Candle;

    fn snap(pairs: &[(&str, Real)]) -> Snapshot<String> {
        pairs
            .iter()
            .map(|(k, close)| {
                (
                    (*k).to_string(),
                    Atom::new(Candle::new(1.0, 1.0, 1.0, *close, 1.0)),
                )
            })
            .collect()
    }

    use crate::types::Real;

    #[test]
    fn picks_present_asset() {
        let mut p = Pick::<String>::new("BTC".into());
        let out = p.update(snap(&[("BTC", 10.0), ("ETH", 20.0)]));
        assert_eq!(out.map(|a| a.candle.close), Some(10.0));
    }

    #[test]
    fn missing_key_yields_none() {
        let mut p = Pick::<String>::new("SOL".into());
        let out = p.update(snap(&[("BTC", 10.0), ("ETH", 20.0)]));
        assert_eq!(out, None);
        assert_eq!(p.value(), None);
    }

    #[test]
    fn warm_up_delegates_to_source() {
        let p = Pick::<String>::new("BTC".into());
        // Identity<Snapshot<K>>::warm_up_period == 1, so Pick::new reports 1.
        assert_eq!(p.warm_up_period(), 1);
    }

    #[test]
    fn reset_clears_cached_value() {
        let mut p = Pick::<String>::new("BTC".into());
        p.update(snap(&[("BTC", 42.0)]));
        assert_eq!(p.value().map(|a| a.candle.close), Some(42.0));
        p.reset();
        assert_eq!(p.value(), None);
    }
}
