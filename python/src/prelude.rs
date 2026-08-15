//! Imports shared by every binding module.
//!
//! The bindings were one 9k-line file; splitting it left each module needing
//! the same set of `use`s. Rather than repeat them eight times, they live here
//! and each module glob-imports this.

#![allow(unused_imports)]

pub(crate) use pyo3::exceptions::{PyTypeError, PyValueError};
pub(crate) use pyo3::prelude::*;
pub(crate) use pyo3::types::PyDict;

pub(crate) use std::sync::{Arc, Mutex};

pub(crate) use fugazi_core::Indicator;
pub(crate) use fugazi_core::indicators::compare::{EqOp, GeOp, GtOp, LeOp, LtOp, NeOp, StrEqOp, StrNeOp};
pub(crate) use fugazi_core::indicators::{
    Ad, Adx, AdxValue, Aroon, AroonValue, Atr, BarsSince, BarsSinceHigh, BarsSinceLow, Bollinger,
    BollingerValue, Cci, Close, Correlation, CurrentBar, Day, DayOfWeek, DayOfYear, Dmi, DmiValue,
    Donchian, DonchianValue, Ema, Every, GarmanKlass, GetBool, GetReal, GetStr, High, Hma, Hour,
    Identity,
    IfElse, IsWeekday, IsWeekend, Keltner, KeltnerValue, Kurtosis, Latch, Log, Low, Macd, MacdValue,
    Median, Mfi, Minute, Month, Obv, Open, Parkinson, Percentile, PercentileRank, Pick, Quarter,
    Resample, Rma, RogersSatchell, Rsi, Sar, Second, Skewness, Sma, StdDev, Stochastic, TrueRange,
    Typical, UnixMillis, UnixSeconds, Value, ValueStr, VarianceRatio, Volume, Vwap, WeekOfYear,
    WilliamsR, Wma, Year, ZScore,
};
pub(crate) use fugazi_core::indicators::{BoolIndicatorExt, Combine, DEFAULT_EPSILON, IndicatorExt};
pub(crate) use fugazi_core::sources::{
    Binance, BinanceVision, Coinbase, CoinGecko, Interval, Okx, SeriesSource,
    SourceError, Timestamp, Yahoo,
};
pub(crate) use fugazi_core::wallet::{
    Ack, SleeveWallet, Order, OrderId, OrderKind, PaperWallet, Reference, Side, Size, Units,
    Wallet, WalletError, external_baseline, own_equity,
};
pub(crate) use fugazi_core::live::{CoinbaseWallet, LiveError, OkxWallet};
pub(crate) use fugazi_core::types::{
    Atom, Candle, Frequency, OverlayInfo, OverlayType, OverlayValue, Real, Schema, SchemaBuilder,
    Selector, Snapshot,
};
pub(crate) use fugazi_core::backtest::{Fill, Rejected, RunReport};
pub(crate) use fugazi_core::indicators::ValueBool;
pub(crate) use fugazi_core::strategies::basket as core_basket;
pub(crate) use fugazi_core::strategies::{
    BasketStrategy, MultiAssetStrategy, PairsStrategy, SingleAssetStrategy,
};
pub(crate) use fugazi_core::metrics as core_metrics;
pub(crate) use fugazi_core::metrics::{DrawdownSegment, Trade};
pub(crate) use fugazi_core::runtime::{self, DynType, DynValue, TypeOf};
// Spec-driven surface: YAML load, evaluate, optimize.
pub(crate) use fugazi_core::spec::StrategySpec as CoreStrategySpec;
pub(crate) use fugazi_core::spec::backtest as spec_backtest;
pub(crate) use fugazi_core::spec::costs::CostConfig;
pub(crate) use fugazi_core::spec::metrics::{self as spec_metrics, Metrics as SpecMetrics};
pub(crate) use fugazi_core::montecarlo::ResampleScheme;
pub(crate) use fugazi_core::spec::montecarlo::{McConfig, run_montecarlo};
pub(crate) use fugazi_core::spec::optimize as spec_optimize;
pub(crate) use fugazi_core::spec::{
    BasketStrategySpec, MultiAssetStrategySpec, PairsStrategySpec, PortfolioSpec, StrategyRef,
};
