//! The [`Signal`] marker trait.
//!
//! A *signal* is a boolean condition over a market [`Candle`] — an
//! [`Indicator`]`<Input = Candle, Output = bool>`. `Signal` is a thin marker over
//! exactly that (blanket-implemented), so every candle-fed comparison, boolean
//! combinator and `bool` leaf is a `Signal` automatically and a strategy can hold
//! one as a plain `Box<dyn Signal>`.
//!
//! The boolean combinators (`and`/`or`/`xor`/`not`/`changed`) and the `value()`
//! view are **not** here — they extend *every* `Indicator<Output = bool>`
//! (regardless of input), so they live on
//! [`BoolIndicatorExt`](crate::indicators::BoolIndicatorExt), the boolean twin of
//! [`IndicatorExt`](crate::indicators::IndicatorExt).

use crate::indicator::Indicator;
use crate::types::Candle;

/// A boolean condition over a [`Candle`]: an
/// [`Indicator`]`<Input = Candle, Output = bool>`.
///
/// This is a marker trait, blanket-implemented for every such indicator, so the
/// candle-fed comparison/logic carriers are `Signal`s for free and a strategy can
/// hold them behind `Box<dyn Signal>`. Like any indicator a signal is `None`
/// until warmed up; read it as a plain `bool` (false until ready) with
/// [`BoolIndicatorExt::is_true`](crate::indicators::BoolIndicatorExt::is_true).
pub trait Signal: Indicator<Input = Candle, Output = bool> {}
impl<I: Indicator<Input = Candle, Output = bool> + ?Sized> Signal for I {}
