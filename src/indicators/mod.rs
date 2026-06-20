//! Concrete indicator implementations.
//!
//! Indicators are the composable *sources* of the crate: each incrementally
//! produces a [`Real`](crate::Real) value (or, for multi-output indicators, a
//! small value struct). Leaf sources [`Value`] (a constant) and [`Identity`]
//! (the raw input) let literals and prices take part in composition.
//!
//! Generic transform operators (arithmetic `Add`/`Sub`/`Mul`/`Div`, the
//! lookback ops `Lag`/`Diff`/`Ratio`, and the rolling extremum
//! `RollingMax`/`RollingMin`) live in [`ops`]; comparison operators, which yield
//! signals, live in [`signals::compare`](crate::signals::compare).
//!
//! Shared lower-level cores keep the math in one place: [`smoothing`]'s
//! `EmaState`/`WilderState` back [`Ema`]/[`Macd`] and [`Rma`]/[`Rsi`]/[`Atr`]/
//! [`Adx`]; [`stats`]'s `WindowStats` backs [`Sma`]/[`StdDev`]/[`Bollinger`] and
//! its `WindowExtreme` backs the rolling extremum and [`Stochastic`].

pub mod ops;

mod ad;
mod adx;
mod atr;
mod bollinger;
mod candle;
mod donchian;
mod ema;
mod identity;
mod macd;
mod mfi;
mod obv;
mod rma;
mod rsi;
mod sma;
mod smoothing;
mod stats;
mod stddev;
mod stochastic;
mod true_range;
mod value;
mod vwap;

pub use ad::Ad;
pub use adx::{Adx, AdxValue};
pub use atr::Atr;
pub use bollinger::{Bollinger, BollingerValue};
pub use candle::{
    CandleField, Close, Current, Field, High, Low, Median, Open, Typical, Volume,
};
pub use donchian::{Donchian, DonchianValue};
pub use ema::Ema;
pub use identity::Identity;
pub use macd::{Macd, MacdValue};
pub use mfi::Mfi;
pub use obv::Obv;
pub use ops::{
    Add, BinaryOp, Combine, Diff, Div, Extreme, ExtremeOp, Lag, Lookback, LookbackOp, MaxOp,
    MinOp, Mul, Ratio, RollingMax, RollingMin, Sub,
};
pub use rma::Rma;
pub use rsi::Rsi;
pub use sma::Sma;
pub use stddev::StdDev;
pub use stochastic::{StochRsi, Stochastic};
pub use true_range::TrueRange;
pub use value::Value;
pub use vwap::Vwap;
