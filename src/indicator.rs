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

    /// The number of samples that must be fed through
    /// [`update`](Indicator::update) before the first output can appear.
    ///
    /// The exact warm-up: `update` returns `None` for the first
    /// `warm_up_period() - 1` samples and (data permitting) `Some` from sample
    /// `warm_up_period()` onwards. Composed indicators account for their
    /// sources, so `Ema::new(Sma::new(src, 10), 20)` reports the warm-up of the
    /// whole chain. `0` means ready without any input (e.g.
    /// [`Value`](crate::indicators::Value)).
    ///
    /// "Data permitting": a degenerate input can delay readiness beyond this —
    /// e.g. a division by zero in a [`Div`](crate::indicators::Div), or
    /// zero-volume bars into a [`Vwap`](crate::indicators::Vwap). And the
    /// position-anchored sources ([`PositionField`](crate::indicators::PositionField))
    /// report `0` because their readiness tracks the live position, not the
    /// sample count.
    fn warm_up_period(&self) -> usize;

    /// The number of *additional* samples after
    /// [`warm_up_period`](Indicator::warm_up_period) before the output has
    /// effectively converged.
    ///
    /// Windowed (FIR) indicators — SMA, rolling extrema, Bollinger, Stochastic,
    /// … — depend only on the last `period` samples, so they are exact as soon
    /// as they are ready and report `0` (the default). Recursive (IIR)
    /// indicators — EMA, RMA/Wilder and everything built on them (RSI, ATR,
    /// ADX, MACD, …) — carry their seed forward forever, so early outputs
    /// depend on *where the stream started*; they report the number of samples
    /// until the seed's residual weight decays below 0.1%, after which the
    /// output no longer meaningfully depends on the seeding (the same idea as
    /// TA-Lib's "unstable period"). Composed indicators propagate their
    /// sources' instability.
    fn unstable_period(&self) -> usize {
        0
    }

    /// Total samples before the output is both available and converged:
    /// `warm_up_period() + unstable_period()`.
    ///
    /// The amount of history to feed before trusting the output — e.g. how many
    /// bars to replay ahead of a live stream, or how many leading outputs of a
    /// backtest to discard.
    fn stable_period(&self) -> usize {
        self.warm_up_period().saturating_add(self.unstable_period())
    }

    /// Clear all state, returning the indicator to its freshly-constructed
    /// condition.
    fn reset(&mut self);

    /// Serialize this indicator's mutable internal state as a JSON value, for
    /// **run resuming** — persisting a run and continuing it later with
    /// bit-identical behavior.
    ///
    /// The default is [`Null`](serde_json::Value::Null): a *stateless* leaf
    /// (`Value`, `Identity`, `Pick`, the `Current*` accessors, …) has nothing to
    /// save. Every indicator that carries state overrides this — in practice via
    /// `#[derive(SaveState)]` (see the `fugazi-derive` crate), which generates a
    /// `save_state_fields` this method forwards to. The payload's *shape* mirrors
    /// the indicator tree (each node keyed by field name, children nested), and
    /// the structure is always rebuilt from the spec before
    /// [`load_state`](Indicator::load_state) replays the values in — so there are
    /// no type tags, position in the tree is the key.
    fn save_state(&self) -> serde_json::Value {
        serde_json::Value::Null
    }

    /// Restore state produced by [`save_state`](Indicator::save_state) on an
    /// identically-constructed indicator. Default: accept and ignore (the
    /// stateless-leaf case). Returns an `Err` describing the mismatch when the
    /// payload doesn't fit this node's shape.
    fn load_state(&mut self, state: &serde_json::Value) -> Result<(), String> {
        let _ = state;
        Ok(())
    }
}
