//! Concrete indicator implementations.
//!
//! Indicators are the composable *sources* of the crate: each incrementally
//! produces a [`Real`](crate::Real) value (or, for multi-output indicators, a
//! small value struct). Leaf sources [`Value`] (a constant) and [`Identity`]
//! (the raw input) let literals and prices take part in composition.
//!
//! Generic transform operators live in [`ops`] (arithmetic `Add`/`Sub`/`Mul`/`Div`/
//! `Pow` and the pairwise `Max`/`Min`, the pointwise `Abs`/`Sign`/`Sqrt`/`Tanh`/
//! `Sigmoid`, the lookback ops `Lag`/`Diff`/`Ratio`, the rolling extremum
//! `RollingMax`/`RollingMin`, and the unbounded `CumSum`/`CumMax`/`CumMin`), with
//! the fluent [`IndicatorExt`] builder in [`ext`]. Boolean conditions are also indicators — those yielding `bool`:
//! comparison operators live in [`compare`] and the boolean connectives /
//! edge detectors in [`logic`].
//!
//! Shared lower-level cores keep the math in one place: `smoothing`'s
//! `EmaState`/`WilderState` back [`Ema`]/[`Macd`] and [`Rma`]/[`Rsi`]/[`Atr`]/
//! [`Adx`]; `stats`'s `WindowStats` backs [`Sma`]/[`StdDev`]/[`Bollinger`],
//! its `WindowExtreme` backs the rolling extremum and [`Stochastic`], and its
//! `WindowCovariance` backs [`Correlation`]/[`Covariance`]/[`Beta`] and
//! [`LinReg`].

pub mod compare;
pub mod ext;
pub mod logic;
pub mod ops;
pub mod sizing;

mod ad;
mod adx;
mod aroon;
mod atr;
mod bars_since;
mod bollinger;
mod book;
mod calendar;
mod candle;
mod cci;
mod component;
mod crosses;
mod dispatch;
mod dmi;
mod donchian;
mod ema;
mod exp;
mod garman_klass;
mod get;
mod hma;
mod identity;
mod if_else;
mod keltner;
mod kurtosis;
mod linreg;
mod log;
mod macd;
mod mfi;
mod obv;
mod pairwise;
mod parkinson;
mod percentile;
mod pick;
mod position;
mod rma;
mod rogers_satchell;
mod rsi;
mod sar;
mod skewness;
mod sma;
mod smoothing;
// Crate-visible rather than private: `crate::metrics` reaches in for the shared
// `quantile_of_sorted` / `cmp_asc` so report-level and rolling percentiles use
// one convention. The stateful cores stay in here; `Moments` is the one
// re-export, because it is what a public `PairStatOp` reads.
pub(crate) mod stats;
mod stddev;
mod stochastic;
mod timeframe;
mod trailing;
mod true_range;
mod unstable;
mod value;
mod variance_ratio;
mod vwap;
mod williams_r;
mod wma;
mod zscore;

pub use ad::Ad;
pub use adx::{Adx, AdxValue};
pub use aroon::{Aroon, AroonValue};
pub use atr::Atr;
pub use bars_since::{BarsSince, BarsSinceHigh, BarsSinceLow};
pub use bollinger::{Bollinger, BollingerValue};
pub use book::{Book, BookField};
pub use calendar::{
    Calendar, CalendarField, CurrentTime, Day, DayOfMonth, DayOfMonthField, DayOfWeek,
    DayOfWeekField, DayOfYear, DayOfYearField, Hour, HourField, IsWeekday, IsWeekend, Minute,
    MinuteField, Month, MonthField, Quarter, QuarterField, Second, SecondField, UnixMillis,
    UnixMillisField, UnixSeconds, UnixSecondsField, WeekOfYear, WeekOfYearField, Year, YearField,
};
pub use candle::{
    CandleField, Close, Current, CurrentBar, Field, High, Low, Median, Open, Typical, Volume,
};
pub use cci::Cci;
pub use compare::{
    ComparisonOp, DEFAULT_TOLERANCE, Eq, Ge, Gt, Le, Lt, Ne, StrEq, StrEqOp, StrNe, StrNeOp,
    Tolerance,
};
pub use component::{Component, Shared, SharedComponent, SharedHandle};
pub use crosses::{CrossesAbove, CrossesBelow};
pub use dispatch::Match;
pub use dmi::{Dmi, DmiValue};
pub use donchian::{Donchian, DonchianValue};
pub use ema::Ema;
pub use exp::Exp;
pub use ext::{BoolIndicatorExt, IndicatorExt};
pub use garman_klass::GarmanKlass;
pub use get::{GetBool, GetError, GetReal, GetStr, TypeMismatch, UnknownKey};
pub use hma::Hma;
pub use identity::Identity;
pub use if_else::IfElse;
pub use keltner::{Keltner, KeltnerValue};
pub use kurtosis::Kurtosis;
pub use linreg::{LinReg, LinRegValue};
pub use log::Log;
pub use logic::{And, BecameFalse, BecameTrue, Change, Every, Not, Or, ValueBool, Xor};
pub use macd::{Macd, MacdValue};
pub use mfi::Mfi;
pub use obv::Obv;
pub use parkinson::Parkinson;
pub use percentile::{Percentile, PercentileRank};
pub use stats::Moments;

pub use ops::{
    Abs, AbsOp, Add, BinaryOp, Combine, CumMax, CumMin, CumSum, Cumulative, CumulativeOp, Diff,
    Div, Extreme, ExtremeOp, Lag, Lookback, LookbackOp, Max, MaxOp, Min, MinOp, Mul, Pow, PowOp,
    Ratio, Roc, RollingMax, RollingMin, Sigmoid, SigmoidOp, Sign, SignOp, Sqrt, SqrtOp, Sub, Tanh,
    TanhOp, Unary, UnaryOp,
};
pub use pairwise::{
    Beta, BetaOp, Correlation, CorrelationOp, Covariance, CovarianceOp, PairStat, PairStatOp,
};
pub use pick::{Pick, PickAny};
pub use position::{Position, PositionField};
pub use rma::Rma;
pub use rogers_satchell::RogersSatchell;
pub use rsi::Rsi;
pub use sar::Sar;
pub use skewness::Skewness;
pub use sma::Sma;
pub use stddev::StdDev;
pub use stochastic::{StochRsi, Stochastic};
pub use timeframe::{
    Accumulate, BarMeasure, DollarBars, DollarMeasure, Latch, Resample, VolumeBars, VolumeMeasure,
};
pub use trailing::{Calmar, MaxDrawdown, Sharpe, Sortino, Volatility};
pub use true_range::TrueRange;
pub use unstable::Unstable;
pub use value::{Value, ValueStr};
pub use variance_ratio::VarianceRatio;
pub use vwap::Vwap;
pub use williams_r::WilliamsR;
pub use wma::Wma;
pub use zscore::ZScore;
