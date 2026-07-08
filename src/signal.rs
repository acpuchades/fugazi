//! The [`Signal`] marker trait.
//!
//! A *signal* is a boolean condition over some `In` per-bar input — an
//! [`Indicator`]`<Input = In, Output = bool>`. `Signal<In>` is a thin marker
//! over exactly that (blanket-implemented), so every `In`-fed comparison,
//! boolean combinator and `bool` leaf is a `Signal<In>` automatically and a
//! strategy can hold one as `Box<dyn Signal<In>>`.
//!
//! Most single-asset strategies parameterise `In` as
//! [`Snapshot<Sym>`](crate::types::Snapshot) — a boolean condition over a
//! multi-asset input frame, with the strategy's own asset unwrapped by an
//! empty-selector [`Pick`](crate::indicators::Pick) inside each leaf. But
//! `Signal<Atom>` also works for direct atom-fed strategies, and `Signal<Real>`
//! for the scalar-stream case.
//!
//! The boolean combinators (`and`/`or`/`xor`/`not`/`changed`) and the
//! `value()` view are **not** here — they extend *every* `Indicator<Output =
//! bool>` (regardless of input), so they live on
//! [`BoolIndicatorExt`](crate::indicators::BoolIndicatorExt), the boolean twin
//! of [`IndicatorExt`](crate::indicators::IndicatorExt).

use crate::indicator::Indicator;

/// A boolean condition over an `In` per-bar input: an
/// [`Indicator`]`<Input = In, Output = bool>`.
///
/// This is a marker trait, blanket-implemented for every such indicator, so
/// the input-agnostic comparison/logic carriers are `Signal<In>`s for free
/// and a strategy can hold them behind `Box<dyn Signal<In>>`. Like any
/// indicator a signal is `None` until warmed up; read it as a plain `bool`
/// (false until ready) with
/// [`BoolIndicatorExt::is_true`](crate::indicators::BoolIndicatorExt::is_true).
pub trait Signal<In>: Indicator<Input = In, Output = bool> {}
impl<In, I: Indicator<Input = In, Output = bool> + ?Sized> Signal<In> for I {}
