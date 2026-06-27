//! The core [`Indicator`] trait.

/// An incremental technical indicator.
///
/// An indicator owns its internal state and is advanced one sample at a time
/// via [`update`](Indicator::update). Many indicators require a warm-up period
/// before they can produce a meaningful value, so outputs are wrapped in
/// `Option`: `None` until enough samples have been observed, `Some` afterwards.
///
/// Concrete indicators additionally expose their latest output(s) as **public
/// fields** that are refreshed on every `update`. A single-output indicator
/// exposes a field named `value` — the same `Option` the
/// [`value`](Indicator::value) method returns; multi-output indicators expose one
/// named field per output and set
/// [`Output`](Indicator::Output) to a struct holding them all.
pub trait Indicator {
    /// The per-sample input — commonly [`Real`](crate::Real) or
    /// [`Candle`](crate::Candle).
    type Input;

    /// The produced output — commonly [`Real`](crate::Real), or a struct for
    /// multi-output indicators.
    type Output: Clone;

    /// Feed the next sample, advancing internal state, and return the current
    /// output if the indicator is warmed up.
    fn update(&mut self, input: Self::Input) -> Option<Self::Output>;

    /// The most recent output, without advancing state.
    fn value(&self) -> Option<Self::Output>;

    /// Whether enough samples have been observed to produce a valid output.
    fn is_ready(&self) -> bool {
        self.value().is_some()
    }

    /// Clear all state, returning the indicator to its freshly-constructed
    /// condition.
    fn reset(&mut self);
}
