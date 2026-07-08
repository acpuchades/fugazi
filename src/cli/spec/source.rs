//! YAML-deserializable [`SourceSpec`] — the real-valued source layer.
//!
//! Split out of `spec/mod.rs`; kept in `crate::spec::source` so paths like
//! `crate::spec::SourceSpec` still resolve via the `pub use` in `mod.rs`.

use std::sync::Arc;

use serde::Deserialize;

// Field / calendar / current-bar / current-time leaves are referenced through
// their full `fugazi::indicators::` paths inside the `SourceSpec::build`
// match arms — the source-spec variants share those names (Close, High, Year,
// …) as enum-variant identifiers, so a bare `Close::of(...)` would try to
// resolve on the enum variant. The `Pick` root is the one exception because
// it isn't a `SourceSpec` variant.
use fugazi::indicators::{
    Ad, Adx, AdxValue, Aroon, AroonValue, Atr, Bollinger, BollingerValue, Cci, Component, Dmi,
    DmiValue, Donchian, DonchianValue, Ema, GetBool, GetReal, GetStr, Hma, Keltner, KeltnerValue,
    Latch, Log, Macd, MacdValue, Mfi, Obv, Pick, Position, Resample, Rma, Rsi, Sar, Sma, StdDev,
    StochRsi, Stochastic, TrueRange, Value, Vwap, WilliamsR, Wma,
};
use fugazi::prelude::*;
use fugazi::types::Snapshot;

use crate::dyn_indicator::{self, AsAtom, AsCandle, AsReal, DynIndicator};

use fugazi::{Frequency, Selector};
use std::str::FromStr;

/// Every atom-input leaf's `source` field defaults to `None`, at which point
/// `atom_source_of` produces this implicit empty-selector [`Pick`] — the
/// single-entry snapshot unpack that keeps single-series strategies working.
/// Multi-asset ones opt in by writing an explicit `!pick { symbol: ... }` as
/// the `source:` of the leaf.
fn pick_root() -> Pick<String> {
    Pick::<String>::new()
}

pub(super) fn default_source() -> Box<SourceSpec> {
    Box::new(SourceSpec::Close { source: None })
}
pub(super) fn default_high() -> Box<SourceSpec> {
    Box::new(SourceSpec::High { source: None })
}
pub(super) fn default_low() -> Box<SourceSpec> {
    Box::new(SourceSpec::Low { source: None })
}
/// Default candle source for bar indicators — the current bar itself.
pub(super) fn default_bar_source() -> Box<SourceSpec> {
    Box::new(SourceSpec::Current { source: None })
}

/// Default base for `!log`: natural log (`e`).
pub(super) fn default_log_base() -> Real {
    std::f64::consts::E
}

// ---------------------------------------------------------------------------
// Real-valued sources
// ---------------------------------------------------------------------------

/// A real-valued source over a candle stream — the YAML form of any
/// `Indicator<Input = Candle, Output = Real>`.
///
/// Every atom-input leaf (`!close`, `!high`, …, all calendar accessors, and
/// `!get`) carries a **defaulted optional `source: Option<Box<SourceSpec>>`**
/// field. When omitted, the leaf reads its atom from the implicit
/// empty-selector [`Pick::<String>::new()`] — the single-entry snapshot
/// unpack that keeps single-series strategies working. When provided
/// (typically a `!pick { symbol, freq }`), the leaf reads from that
/// atom-emitting subtree, which is how cross-asset composition is spelled:
///
/// ```yaml
/// # BTC vs ETH close spread:
/// !sub
///   lhs: !close { source: !pick { symbol: BTC } }
///   rhs: !close { source: !pick { symbol: ETH } }
/// ```
///
/// Three input forms all deserialize to the same variant:
/// - A bare word — `close`
/// - A bare YAML tag — `!close`
/// - A tagged map — `!close { source: !pick { symbol: BTC } }`
///
/// The bare-word / bare-tag forms use the implicit `Pick` root; the tagged
/// map form threads the given atom source through the leaf. The custom
/// [`TryFrom<serde_norway::Value>`] impl below normalises the string and
/// tagged shapes into the map shape [`SourceSpecRaw`] expects, and
/// [`SourceSpecRaw`] carries the derived externally-tagged deserialization
/// logic.
#[derive(Debug, Clone, Deserialize)]
#[serde(try_from = "serde_norway::Value")]
pub enum SourceSpec {
    // --- atom-input leaves (candle fields) ---
    Close {
        #[serde(default)]
        source: Option<Box<SourceSpec>>,
    },
    High {
        #[serde(default)]
        source: Option<Box<SourceSpec>>,
    },
    Low {
        #[serde(default)]
        source: Option<Box<SourceSpec>>,
    },
    Open {
        #[serde(default)]
        source: Option<Box<SourceSpec>>,
    },
    Volume {
        #[serde(default)]
        source: Option<Box<SourceSpec>>,
    },
    Typical {
        #[serde(default)]
        source: Option<Box<SourceSpec>>,
    },
    Median {
        #[serde(default)]
        source: Option<Box<SourceSpec>>,
    },
    /// The current bar itself — the whole [`Candle`], not a scalar. The default
    /// bar source of every bar-consuming indicator (`!atr`, `!obv`, `!adx`, …);
    /// wrap in [`SourceSpec::Resample`] to lift a downstream bar indicator
    /// onto a higher timeframe.
    Current {
        #[serde(default)]
        source: Option<Box<SourceSpec>>,
    },

    /// Cross-asset projection: project one asset's [`Atom`] out of the
    /// snapshot the CLI feeds each bar. Both fields are optional — an empty
    /// `!pick {}` behaves identically to the implicit single-entry unpack
    /// every atom-input leaf uses by default. Compose with any atom-input
    /// leaf via `source: !pick { symbol, freq }`.
    ///
    /// `freq` accepts the same `N<unit>` alphabet as `--frequency`
    /// (`1m` / `4h` / `1d` / `1w` / `1M`), so a cross-frequency snapshot
    /// disambiguates via `!pick { symbol: BTC, freq: 1h }`.
    Pick {
        #[serde(default)]
        symbol: Option<String>,
        #[serde(default)]
        freq: Option<String>,
    },

    /// A constant value.
    Value(Real),

    /// The current position's entry price — a [`SingleAssetStrategy`] anchor,
    /// for building stop-loss / take-profit levels.
    Entry,
    /// The running high since entry (a long trailing-stop anchor).
    Peak,
    /// The running low since entry (a short trailing-stop anchor).
    Trough,

    /// Read one overlay column by name from each atom's side-channel data.
    ///
    /// The column's declared [`OverlayType`] in the atom stream's schema
    /// picks the output type at build time: a `Real` column yields a
    /// `Real`-output source (fits everywhere a numeric source does), a
    /// `Bool` column yields a `Bool`-output source (fits in any signal
    /// position — `!get` reads as a signal directly), a `Str` column yields
    /// a `Str`-output source (feeds into `!str_eq` / `!str_ne` on the
    /// signal side).
    ///
    /// Builds panic on an unknown key or a type mismatch — a `Str` column
    /// in a Real-typed source position is caught downstream at `AsReal::new`
    /// with the "expected Real" panic, the same failure mode as any other
    /// type-clashed spec.
    Get {
        key: String,
        #[serde(default)]
        source: Option<Box<SourceSpec>>,
    },

    // --- price-series indicators (a source + parameters) ---
    Ema {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        period: usize,
    },
    Sma {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        period: usize,
    },
    Rma {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        period: usize,
    },
    Wma {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        period: usize,
    },
    Hma {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        period: usize,
    },
    Rsi {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        period: usize,
    },
    #[serde(rename = "stddev")]
    StdDev {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        period: usize,
    },
    Cci {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        period: usize,
    },
    Stochastic {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        period: usize,
    },
    StochRsi {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        rsi_period: usize,
        stoch_period: usize,
    },

    // --- multi-output indicators, one variant per component ---
    MacdLine {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        fast: usize,
        slow: usize,
        signal: usize,
    },
    MacdSignal {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        fast: usize,
        slow: usize,
        signal: usize,
    },
    MacdHistogram {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        fast: usize,
        slow: usize,
        signal: usize,
    },
    BbUpper {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        period: usize,
        k: Real,
    },
    BbMiddle {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        period: usize,
        k: Real,
    },
    BbLower {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        period: usize,
        k: Real,
    },
    KeltnerUpper {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        #[serde(default = "default_bar_source")]
        candle_source: Box<SourceSpec>,
        ema_period: usize,
        atr_period: usize,
        multiplier: Real,
    },
    KeltnerMiddle {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        #[serde(default = "default_bar_source")]
        candle_source: Box<SourceSpec>,
        ema_period: usize,
        atr_period: usize,
        multiplier: Real,
    },
    KeltnerLower {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        #[serde(default = "default_bar_source")]
        candle_source: Box<SourceSpec>,
        ema_period: usize,
        atr_period: usize,
        multiplier: Real,
    },
    DonchianUpper {
        #[serde(default = "default_high")]
        high: Box<SourceSpec>,
        #[serde(default = "default_low")]
        low: Box<SourceSpec>,
        period: usize,
    },
    DonchianMiddle {
        #[serde(default = "default_high")]
        high: Box<SourceSpec>,
        #[serde(default = "default_low")]
        low: Box<SourceSpec>,
        period: usize,
    },
    DonchianLower {
        #[serde(default = "default_high")]
        high: Box<SourceSpec>,
        #[serde(default = "default_low")]
        low: Box<SourceSpec>,
        period: usize,
    },
    Adx {
        #[serde(default = "default_bar_source")]
        source: Box<SourceSpec>,
        period: usize,
    },
    PlusDi {
        #[serde(default = "default_bar_source")]
        source: Box<SourceSpec>,
        period: usize,
    },
    MinusDi {
        #[serde(default = "default_bar_source")]
        source: Box<SourceSpec>,
        period: usize,
    },
    DmiPlusDi {
        #[serde(default = "default_bar_source")]
        source: Box<SourceSpec>,
        period: usize,
    },
    DmiMinusDi {
        #[serde(default = "default_bar_source")]
        source: Box<SourceSpec>,
        period: usize,
    },
    AroonUp {
        #[serde(default = "default_bar_source")]
        source: Box<SourceSpec>,
        period: usize,
    },
    AroonDown {
        #[serde(default = "default_bar_source")]
        source: Box<SourceSpec>,
        period: usize,
    },
    AroonOscillator {
        #[serde(default = "default_bar_source")]
        source: Box<SourceSpec>,
        period: usize,
    },

    // --- single-output bar indicators ---
    Atr {
        #[serde(default = "default_bar_source")]
        source: Box<SourceSpec>,
        period: usize,
    },
    Mfi {
        #[serde(default = "default_bar_source")]
        source: Box<SourceSpec>,
        period: usize,
    },
    WilliamsR {
        #[serde(default = "default_bar_source")]
        source: Box<SourceSpec>,
        period: usize,
    },
    Obv {
        #[serde(default = "default_bar_source")]
        source: Box<SourceSpec>,
    },
    Vwap {
        #[serde(default = "default_bar_source")]
        source: Box<SourceSpec>,
    },
    Ad {
        #[serde(default = "default_bar_source")]
        source: Box<SourceSpec>,
    },
    TrueRange {
        #[serde(default = "default_bar_source")]
        source: Box<SourceSpec>,
    },
    Sar {
        #[serde(default = "default_bar_source")]
        source: Box<SourceSpec>,
        step: Real,
        max: Real,
    },

    // --- sizing helpers (real-valued, single-series; read the strategy's
    // own asset via the implicit empty-selector `Pick`). Meant for the
    // `sizing:` slot on `StrategySpec` / `PairsStrategySpec`, but usable
    // anywhere a real-valued source fits.
    /// Inverse realized-vol sizing —
    /// `target / (stddev(log_returns(close), window) * sqrt(bars_per_year))`.
    /// See [`fugazi::indicators::sizing::vol_target`].
    VolTarget {
        target: Real,
        window: usize,
        bars_per_year: Real,
    },
    /// Fixed per-trade risk sized by ATR —
    /// `risk_frac * close / (atr_multiple * ATR(period))`. See
    /// [`fugazi::indicators::sizing::atr_risk`].
    AtrRisk {
        risk_frac: Real,
        period: usize,
        atr_multiple: Real,
    },

    // --- transform operators ---
    Add {
        lhs: Box<SourceSpec>,
        rhs: Box<SourceSpec>,
    },
    Sub {
        lhs: Box<SourceSpec>,
        rhs: Box<SourceSpec>,
    },
    Mul {
        lhs: Box<SourceSpec>,
        rhs: Box<SourceSpec>,
    },
    Div {
        lhs: Box<SourceSpec>,
        rhs: Box<SourceSpec>,
    },
    Lag {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        periods: usize,
    },
    Diff {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        periods: usize,
    },
    Ratio {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        periods: usize,
    },
    Roc {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        periods: usize,
    },
    RollingMax {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        period: usize,
    },
    RollingMin {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        period: usize,
    },
    /// Logarithm of `source` in `base` (defaults to natural log, `e`).
    /// Emits `None` on samples where the source's output is non-positive.
    Log {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        #[serde(default = "default_log_base")]
        base: Real,
    },
    /// Holds the most recent `Some` output of `source`, re-emitting it on
    /// ticks where `source` returns `None`. Wrap the outermost recursive
    /// smoother of a resampled pipeline so per-base-tick consumers see the
    /// finished higher-timeframe value between boundaries — see
    /// [`fugazi::indicators::Latch`].
    Latch { source: Box<SourceSpec> },
    /// Aggregates `every` base candles into one higher-timeframe candle and
    /// runs the `inner` source over it, emitting `inner`'s output on each
    /// completed bucket and `None` in between. `inner` is any source that
    /// reads a candle (`close`/`high`/`typical`, `!ema { period: N, source:
    /// close }`, `!add { lhs, rhs }`, …); it advances only on emissions from
    /// the resample, so an `!ema` inside `!resample` recurses over the HTF
    /// closes, not the base ones. **The resample's clock stays
    /// base-timeframe**: it's fed one base candle per tick and reports at
    /// that same cadence; the emitted `Option<Real>` marks whether the inner
    /// produced a value on a completed bucket. Wrap the whole downstream
    /// chain in [`Latch`](SourceSpec::Latch) so per-base-tick reads see the
    /// finished value between boundaries.
    Resample {
        every: usize,
        inner: Box<SourceSpec>,
        #[serde(default = "default_bar_source")]
        source: Box<SourceSpec>,
    },
    /// Passthrough wrapper that reports `unstable_period() = 0`. The output
    /// and warm-up of `source` are unchanged; the strategy-readiness gate
    /// (which counts up to `stable_period()`) no longer waits for this
    /// subtree's IIR settling tail. The explicit opt-out to the "wait for
    /// every source to be past its unstable tail" safe default; see
    /// [`fugazi::indicators::Unstable`].
    Unstable { source: Box<SourceSpec> },

    // --- calendar accessors (read `atom.time`, emit Real; None when time is
    // absent). Each takes an optional `source` for cross-asset use — the
    // bare form (`!year`) is the default single-series shortcut,
    // `!year { source: !pick { ... } }` reads the picked asset's time.
    /// The Gregorian year (e.g. `2024.0`).
    Year {
        #[serde(default)]
        source: Option<Box<SourceSpec>>,
    },
    /// The Gregorian month, `1.0` (Jan) through `12.0` (Dec).
    Month {
        #[serde(default)]
        source: Option<Box<SourceSpec>>,
    },
    /// The day of the month, `1.0` through `31.0`.
    Day {
        #[serde(default)]
        source: Option<Box<SourceSpec>>,
    },
    /// The hour of the day (UTC), `0.0` through `23.0`.
    Hour {
        #[serde(default)]
        source: Option<Box<SourceSpec>>,
    },
    /// The minute of the hour, `0.0` through `59.0`.
    Minute {
        #[serde(default)]
        source: Option<Box<SourceSpec>>,
    },
    /// The second of the minute, `0.0` through `59.0`.
    Second {
        #[serde(default)]
        source: Option<Box<SourceSpec>>,
    },
    /// ISO 8601 weekday, `1.0` (Monday) through `7.0` (Sunday).
    DayOfWeek {
        #[serde(default)]
        source: Option<Box<SourceSpec>>,
    },
    /// Day of the year, `1.0` through `366.0`.
    DayOfYear {
        #[serde(default)]
        source: Option<Box<SourceSpec>>,
    },
    /// ISO 8601 week of the year, `1.0` through `53.0`.
    WeekOfYear {
        #[serde(default)]
        source: Option<Box<SourceSpec>>,
    },
    /// Calendar quarter, `1.0` through `4.0`.
    Quarter {
        #[serde(default)]
        source: Option<Box<SourceSpec>>,
    },
    /// Unix seconds since the epoch (as a Real).
    UnixSeconds {
        #[serde(default)]
        source: Option<Box<SourceSpec>>,
    },
    /// Unix milliseconds since the epoch (as a Real).
    UnixMillis {
        #[serde(default)]
        source: Option<Box<SourceSpec>>,
    },
    /// The raw bar-open [`Timestamp`] payload (yields
    /// `DynType::Time`, not a scalar). The `Timestamp` twin of
    /// [`SourceSpec::Current`].
    Time {
        #[serde(default)]
        source: Option<Box<SourceSpec>>,
    },
}

// Mirror enum: identical shape as SourceSpec but with derived Deserialize —
// used inside TryFrom<serde_norway::Value> to run the standard externally-
// tagged deserialization once bare-string / tagged shapes are normalised.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
enum SourceSpecRaw {

    // --- atom-input leaves (candle fields) ---
    Close {
        #[serde(default)]
        source: Option<Box<SourceSpec>>,
    },
    High {
        #[serde(default)]
        source: Option<Box<SourceSpec>>,
    },
    Low {
        #[serde(default)]
        source: Option<Box<SourceSpec>>,
    },
    Open {
        #[serde(default)]
        source: Option<Box<SourceSpec>>,
    },
    Volume {
        #[serde(default)]
        source: Option<Box<SourceSpec>>,
    },
    Typical {
        #[serde(default)]
        source: Option<Box<SourceSpec>>,
    },
    Median {
        #[serde(default)]
        source: Option<Box<SourceSpec>>,
    },
    /// The current bar itself — the whole [`Candle`], not a scalar. The default
    /// bar source of every bar-consuming indicator (`!atr`, `!obv`, `!adx`, …);
    /// wrap in [`SourceSpec::Resample`] to lift a downstream bar indicator
    /// onto a higher timeframe.
    Current {
        #[serde(default)]
        source: Option<Box<SourceSpec>>,
    },

    /// Cross-asset projection: project one asset's [`Atom`] out of the
    /// snapshot the CLI feeds each bar. Both fields are optional — an empty
    /// `!pick {}` behaves identically to the implicit single-entry unpack
    /// every atom-input leaf uses by default. Compose with any atom-input
    /// leaf via `source: !pick { symbol, freq }`.
    ///
    /// `freq` accepts the same `N<unit>` alphabet as `--frequency`
    /// (`1m` / `4h` / `1d` / `1w` / `1M`), so a cross-frequency snapshot
    /// disambiguates via `!pick { symbol: BTC, freq: 1h }`.
    Pick {
        #[serde(default)]
        symbol: Option<String>,
        #[serde(default)]
        freq: Option<String>,
    },

    /// A constant value.
    Value(Real),

    /// The current position's entry price — a [`SingleAssetStrategy`] anchor,
    /// for building stop-loss / take-profit levels.
    Entry,
    /// The running high since entry (a long trailing-stop anchor).
    Peak,
    /// The running low since entry (a short trailing-stop anchor).
    Trough,

    /// Read one overlay column by name from each atom's side-channel data.
    ///
    /// The column's declared [`OverlayType`] in the atom stream's schema
    /// picks the output type at build time: a `Real` column yields a
    /// `Real`-output source (fits everywhere a numeric source does), a
    /// `Bool` column yields a `Bool`-output source (fits in any signal
    /// position — `!get` reads as a signal directly), a `Str` column yields
    /// a `Str`-output source (feeds into `!str_eq` / `!str_ne` on the
    /// signal side).
    ///
    /// Builds panic on an unknown key or a type mismatch — a `Str` column
    /// in a Real-typed source position is caught downstream at `AsReal::new`
    /// with the "expected Real" panic, the same failure mode as any other
    /// type-clashed spec.
    Get {
        key: String,
        #[serde(default)]
        source: Option<Box<SourceSpec>>,
    },

    // --- price-series indicators (a source + parameters) ---
    Ema {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        period: usize,
    },
    Sma {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        period: usize,
    },
    Rma {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        period: usize,
    },
    Wma {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        period: usize,
    },
    Hma {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        period: usize,
    },
    Rsi {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        period: usize,
    },
    #[serde(rename = "stddev")]
    StdDev {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        period: usize,
    },
    Cci {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        period: usize,
    },
    Stochastic {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        period: usize,
    },
    StochRsi {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        rsi_period: usize,
        stoch_period: usize,
    },

    // --- multi-output indicators, one variant per component ---
    MacdLine {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        fast: usize,
        slow: usize,
        signal: usize,
    },
    MacdSignal {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        fast: usize,
        slow: usize,
        signal: usize,
    },
    MacdHistogram {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        fast: usize,
        slow: usize,
        signal: usize,
    },
    BbUpper {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        period: usize,
        k: Real,
    },
    BbMiddle {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        period: usize,
        k: Real,
    },
    BbLower {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        period: usize,
        k: Real,
    },
    KeltnerUpper {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        #[serde(default = "default_bar_source")]
        candle_source: Box<SourceSpec>,
        ema_period: usize,
        atr_period: usize,
        multiplier: Real,
    },
    KeltnerMiddle {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        #[serde(default = "default_bar_source")]
        candle_source: Box<SourceSpec>,
        ema_period: usize,
        atr_period: usize,
        multiplier: Real,
    },
    KeltnerLower {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        #[serde(default = "default_bar_source")]
        candle_source: Box<SourceSpec>,
        ema_period: usize,
        atr_period: usize,
        multiplier: Real,
    },
    DonchianUpper {
        #[serde(default = "default_high")]
        high: Box<SourceSpec>,
        #[serde(default = "default_low")]
        low: Box<SourceSpec>,
        period: usize,
    },
    DonchianMiddle {
        #[serde(default = "default_high")]
        high: Box<SourceSpec>,
        #[serde(default = "default_low")]
        low: Box<SourceSpec>,
        period: usize,
    },
    DonchianLower {
        #[serde(default = "default_high")]
        high: Box<SourceSpec>,
        #[serde(default = "default_low")]
        low: Box<SourceSpec>,
        period: usize,
    },
    Adx {
        #[serde(default = "default_bar_source")]
        source: Box<SourceSpec>,
        period: usize,
    },
    PlusDi {
        #[serde(default = "default_bar_source")]
        source: Box<SourceSpec>,
        period: usize,
    },
    MinusDi {
        #[serde(default = "default_bar_source")]
        source: Box<SourceSpec>,
        period: usize,
    },
    DmiPlusDi {
        #[serde(default = "default_bar_source")]
        source: Box<SourceSpec>,
        period: usize,
    },
    DmiMinusDi {
        #[serde(default = "default_bar_source")]
        source: Box<SourceSpec>,
        period: usize,
    },
    AroonUp {
        #[serde(default = "default_bar_source")]
        source: Box<SourceSpec>,
        period: usize,
    },
    AroonDown {
        #[serde(default = "default_bar_source")]
        source: Box<SourceSpec>,
        period: usize,
    },
    AroonOscillator {
        #[serde(default = "default_bar_source")]
        source: Box<SourceSpec>,
        period: usize,
    },

    // --- single-output bar indicators ---
    Atr {
        #[serde(default = "default_bar_source")]
        source: Box<SourceSpec>,
        period: usize,
    },
    Mfi {
        #[serde(default = "default_bar_source")]
        source: Box<SourceSpec>,
        period: usize,
    },
    WilliamsR {
        #[serde(default = "default_bar_source")]
        source: Box<SourceSpec>,
        period: usize,
    },
    Obv {
        #[serde(default = "default_bar_source")]
        source: Box<SourceSpec>,
    },
    Vwap {
        #[serde(default = "default_bar_source")]
        source: Box<SourceSpec>,
    },
    Ad {
        #[serde(default = "default_bar_source")]
        source: Box<SourceSpec>,
    },
    TrueRange {
        #[serde(default = "default_bar_source")]
        source: Box<SourceSpec>,
    },
    Sar {
        #[serde(default = "default_bar_source")]
        source: Box<SourceSpec>,
        step: Real,
        max: Real,
    },

    // --- sizing helpers (real-valued, single-series; read the strategy's
    // own asset via the implicit empty-selector `Pick`). Meant for the
    // `sizing:` slot on `StrategySpec` / `PairsStrategySpec`, but usable
    // anywhere a real-valued source fits.
    /// Inverse realized-vol sizing —
    /// `target / (stddev(log_returns(close), window) * sqrt(bars_per_year))`.
    /// See [`fugazi::indicators::sizing::vol_target`].
    VolTarget {
        target: Real,
        window: usize,
        bars_per_year: Real,
    },
    /// Fixed per-trade risk sized by ATR —
    /// `risk_frac * close / (atr_multiple * ATR(period))`. See
    /// [`fugazi::indicators::sizing::atr_risk`].
    AtrRisk {
        risk_frac: Real,
        period: usize,
        atr_multiple: Real,
    },

    // --- transform operators ---
    Add {
        lhs: Box<SourceSpec>,
        rhs: Box<SourceSpec>,
    },
    Sub {
        lhs: Box<SourceSpec>,
        rhs: Box<SourceSpec>,
    },
    Mul {
        lhs: Box<SourceSpec>,
        rhs: Box<SourceSpec>,
    },
    Div {
        lhs: Box<SourceSpec>,
        rhs: Box<SourceSpec>,
    },
    Lag {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        periods: usize,
    },
    Diff {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        periods: usize,
    },
    Ratio {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        periods: usize,
    },
    Roc {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        periods: usize,
    },
    RollingMax {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        period: usize,
    },
    RollingMin {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        period: usize,
    },
    /// Logarithm of `source` in `base` (defaults to natural log, `e`).
    /// Emits `None` on samples where the source's output is non-positive.
    Log {
        #[serde(default = "default_source")]
        source: Box<SourceSpec>,
        #[serde(default = "default_log_base")]
        base: Real,
    },
    /// Holds the most recent `Some` output of `source`, re-emitting it on
    /// ticks where `source` returns `None`. Wrap the outermost recursive
    /// smoother of a resampled pipeline so per-base-tick consumers see the
    /// finished higher-timeframe value between boundaries — see
    /// [`fugazi::indicators::Latch`].
    Latch { source: Box<SourceSpec> },
    /// Aggregates `every` base candles into one higher-timeframe candle and
    /// runs the `inner` source over it, emitting `inner`'s output on each
    /// completed bucket and `None` in between. `inner` is any source that
    /// reads a candle (`close`/`high`/`typical`, `!ema { period: N, source:
    /// close }`, `!add { lhs, rhs }`, …); it advances only on emissions from
    /// the resample, so an `!ema` inside `!resample` recurses over the HTF
    /// closes, not the base ones. **The resample's clock stays
    /// base-timeframe**: it's fed one base candle per tick and reports at
    /// that same cadence; the emitted `Option<Real>` marks whether the inner
    /// produced a value on a completed bucket. Wrap the whole downstream
    /// chain in [`Latch`](SourceSpec::Latch) so per-base-tick reads see the
    /// finished value between boundaries.
    Resample {
        every: usize,
        inner: Box<SourceSpec>,
        #[serde(default = "default_bar_source")]
        source: Box<SourceSpec>,
    },
    /// Passthrough wrapper that reports `unstable_period() = 0`. The output
    /// and warm-up of `source` are unchanged; the strategy-readiness gate
    /// (which counts up to `stable_period()`) no longer waits for this
    /// subtree's IIR settling tail. The explicit opt-out to the "wait for
    /// every source to be past its unstable tail" safe default; see
    /// [`fugazi::indicators::Unstable`].
    Unstable { source: Box<SourceSpec> },

    // --- calendar accessors (read `atom.time`, emit Real; None when time is
    // absent). Each takes an optional `source` for cross-asset use — the
    // bare form (`!year`) is the default single-series shortcut,
    // `!year { source: !pick { ... } }` reads the picked asset's time.
    /// The Gregorian year (e.g. `2024.0`).
    Year {
        #[serde(default)]
        source: Option<Box<SourceSpec>>,
    },
    /// The Gregorian month, `1.0` (Jan) through `12.0` (Dec).
    Month {
        #[serde(default)]
        source: Option<Box<SourceSpec>>,
    },
    /// The day of the month, `1.0` through `31.0`.
    Day {
        #[serde(default)]
        source: Option<Box<SourceSpec>>,
    },
    /// The hour of the day (UTC), `0.0` through `23.0`.
    Hour {
        #[serde(default)]
        source: Option<Box<SourceSpec>>,
    },
    /// The minute of the hour, `0.0` through `59.0`.
    Minute {
        #[serde(default)]
        source: Option<Box<SourceSpec>>,
    },
    /// The second of the minute, `0.0` through `59.0`.
    Second {
        #[serde(default)]
        source: Option<Box<SourceSpec>>,
    },
    /// ISO 8601 weekday, `1.0` (Monday) through `7.0` (Sunday).
    DayOfWeek {
        #[serde(default)]
        source: Option<Box<SourceSpec>>,
    },
    /// Day of the year, `1.0` through `366.0`.
    DayOfYear {
        #[serde(default)]
        source: Option<Box<SourceSpec>>,
    },
    /// ISO 8601 week of the year, `1.0` through `53.0`.
    WeekOfYear {
        #[serde(default)]
        source: Option<Box<SourceSpec>>,
    },
    /// Calendar quarter, `1.0` through `4.0`.
    Quarter {
        #[serde(default)]
        source: Option<Box<SourceSpec>>,
    },
    /// Unix seconds since the epoch (as a Real).
    UnixSeconds {
        #[serde(default)]
        source: Option<Box<SourceSpec>>,
    },
    /// Unix milliseconds since the epoch (as a Real).
    UnixMillis {
        #[serde(default)]
        source: Option<Box<SourceSpec>>,
    },
    /// The raw bar-open [`Timestamp`] payload (yields
    /// `DynType::Time`, not a scalar). The `Timestamp` twin of
    /// [`SourceSpec::Current`].
    Time {
        #[serde(default)]
        source: Option<Box<SourceSpec>>,
    },
}

impl From<SourceSpecRaw> for SourceSpec {
    fn from(v: SourceSpecRaw) -> Self {
        match v {
            SourceSpecRaw::Close { source } => SourceSpec::Close { source },
            SourceSpecRaw::High { source } => SourceSpec::High { source },
            SourceSpecRaw::Low { source } => SourceSpec::Low { source },
            SourceSpecRaw::Open { source } => SourceSpec::Open { source },
            SourceSpecRaw::Volume { source } => SourceSpec::Volume { source },
            SourceSpecRaw::Typical { source } => SourceSpec::Typical { source },
            SourceSpecRaw::Median { source } => SourceSpec::Median { source },
            SourceSpecRaw::Current { source } => SourceSpec::Current { source },
            SourceSpecRaw::Pick { symbol, freq } => SourceSpec::Pick { symbol, freq },
            SourceSpecRaw::Value(x) => SourceSpec::Value(x),
            SourceSpecRaw::Entry => SourceSpec::Entry,
            SourceSpecRaw::Peak => SourceSpec::Peak,
            SourceSpecRaw::Trough => SourceSpec::Trough,
            SourceSpecRaw::Get { key, source } => SourceSpec::Get { key, source },
            SourceSpecRaw::Ema { source, period } => SourceSpec::Ema { source, period },
            SourceSpecRaw::Sma { source, period } => SourceSpec::Sma { source, period },
            SourceSpecRaw::Rma { source, period } => SourceSpec::Rma { source, period },
            SourceSpecRaw::Wma { source, period } => SourceSpec::Wma { source, period },
            SourceSpecRaw::Hma { source, period } => SourceSpec::Hma { source, period },
            SourceSpecRaw::Rsi { source, period } => SourceSpec::Rsi { source, period },
            SourceSpecRaw::StdDev { source, period } => SourceSpec::StdDev { source, period },
            SourceSpecRaw::Cci { source, period } => SourceSpec::Cci { source, period },
            SourceSpecRaw::Stochastic { source, period } => SourceSpec::Stochastic { source, period },
            SourceSpecRaw::StochRsi { source, rsi_period, stoch_period } => SourceSpec::StochRsi { source, rsi_period, stoch_period },
            SourceSpecRaw::MacdLine { source, fast, slow, signal } => SourceSpec::MacdLine { source, fast, slow, signal },
            SourceSpecRaw::MacdSignal { source, fast, slow, signal } => SourceSpec::MacdSignal { source, fast, slow, signal },
            SourceSpecRaw::MacdHistogram { source, fast, slow, signal } => SourceSpec::MacdHistogram { source, fast, slow, signal },
            SourceSpecRaw::BbUpper { source, period, k } => SourceSpec::BbUpper { source, period, k },
            SourceSpecRaw::BbMiddle { source, period, k } => SourceSpec::BbMiddle { source, period, k },
            SourceSpecRaw::BbLower { source, period, k } => SourceSpec::BbLower { source, period, k },
            SourceSpecRaw::KeltnerUpper { source, candle_source, ema_period, atr_period, multiplier } => SourceSpec::KeltnerUpper { source, candle_source, ema_period, atr_period, multiplier },
            SourceSpecRaw::KeltnerMiddle { source, candle_source, ema_period, atr_period, multiplier } => SourceSpec::KeltnerMiddle { source, candle_source, ema_period, atr_period, multiplier },
            SourceSpecRaw::KeltnerLower { source, candle_source, ema_period, atr_period, multiplier } => SourceSpec::KeltnerLower { source, candle_source, ema_period, atr_period, multiplier },
            SourceSpecRaw::DonchianUpper { high, low, period } => SourceSpec::DonchianUpper { high, low, period },
            SourceSpecRaw::DonchianMiddle { high, low, period } => SourceSpec::DonchianMiddle { high, low, period },
            SourceSpecRaw::DonchianLower { high, low, period } => SourceSpec::DonchianLower { high, low, period },
            SourceSpecRaw::Adx { source, period } => SourceSpec::Adx { source, period },
            SourceSpecRaw::PlusDi { source, period } => SourceSpec::PlusDi { source, period },
            SourceSpecRaw::MinusDi { source, period } => SourceSpec::MinusDi { source, period },
            SourceSpecRaw::DmiPlusDi { source, period } => SourceSpec::DmiPlusDi { source, period },
            SourceSpecRaw::DmiMinusDi { source, period } => SourceSpec::DmiMinusDi { source, period },
            SourceSpecRaw::AroonUp { source, period } => SourceSpec::AroonUp { source, period },
            SourceSpecRaw::AroonDown { source, period } => SourceSpec::AroonDown { source, period },
            SourceSpecRaw::AroonOscillator { source, period } => SourceSpec::AroonOscillator { source, period },
            SourceSpecRaw::Atr { source, period } => SourceSpec::Atr { source, period },
            SourceSpecRaw::Mfi { source, period } => SourceSpec::Mfi { source, period },
            SourceSpecRaw::WilliamsR { source, period } => SourceSpec::WilliamsR { source, period },
            SourceSpecRaw::Obv { source } => SourceSpec::Obv { source },
            SourceSpecRaw::Vwap { source } => SourceSpec::Vwap { source },
            SourceSpecRaw::Ad { source } => SourceSpec::Ad { source },
            SourceSpecRaw::TrueRange { source } => SourceSpec::TrueRange { source },
            SourceSpecRaw::Sar { source, step, max } => SourceSpec::Sar { source, step, max },
            SourceSpecRaw::VolTarget { target, window, bars_per_year } => SourceSpec::VolTarget { target, window, bars_per_year },
            SourceSpecRaw::AtrRisk { risk_frac, period, atr_multiple } => SourceSpec::AtrRisk { risk_frac, period, atr_multiple },
            SourceSpecRaw::Add { lhs, rhs } => SourceSpec::Add { lhs, rhs },
            SourceSpecRaw::Sub { lhs, rhs } => SourceSpec::Sub { lhs, rhs },
            SourceSpecRaw::Mul { lhs, rhs } => SourceSpec::Mul { lhs, rhs },
            SourceSpecRaw::Div { lhs, rhs } => SourceSpec::Div { lhs, rhs },
            SourceSpecRaw::Lag { source, periods } => SourceSpec::Lag { source, periods },
            SourceSpecRaw::Diff { source, periods } => SourceSpec::Diff { source, periods },
            SourceSpecRaw::Ratio { source, periods } => SourceSpec::Ratio { source, periods },
            SourceSpecRaw::Roc { source, periods } => SourceSpec::Roc { source, periods },
            SourceSpecRaw::RollingMax { source, period } => SourceSpec::RollingMax { source, period },
            SourceSpecRaw::RollingMin { source, period } => SourceSpec::RollingMin { source, period },
            SourceSpecRaw::Log { source, base } => SourceSpec::Log { source, base },
            SourceSpecRaw::Latch { source } => SourceSpec::Latch { source },
            SourceSpecRaw::Resample { every, inner, source } => SourceSpec::Resample { every, inner, source },
            SourceSpecRaw::Unstable { source } => SourceSpec::Unstable { source },
            SourceSpecRaw::Year { source } => SourceSpec::Year { source },
            SourceSpecRaw::Month { source } => SourceSpec::Month { source },
            SourceSpecRaw::Day { source } => SourceSpec::Day { source },
            SourceSpecRaw::Hour { source } => SourceSpec::Hour { source },
            SourceSpecRaw::Minute { source } => SourceSpec::Minute { source },
            SourceSpecRaw::Second { source } => SourceSpec::Second { source },
            SourceSpecRaw::DayOfWeek { source } => SourceSpec::DayOfWeek { source },
            SourceSpecRaw::DayOfYear { source } => SourceSpec::DayOfYear { source },
            SourceSpecRaw::WeekOfYear { source } => SourceSpec::WeekOfYear { source },
            SourceSpecRaw::Quarter { source } => SourceSpec::Quarter { source },
            SourceSpecRaw::UnixSeconds { source } => SourceSpec::UnixSeconds { source },
            SourceSpecRaw::UnixMillis { source } => SourceSpec::UnixMillis { source },
            SourceSpecRaw::Time { source } => SourceSpec::Time { source },
        }
    }
}

impl TryFrom<serde_norway::Value> for SourceSpec {
    type Error = String;

    /// Normalise the incoming YAML value into a [`serde_norway::Value::Tagged`],
    /// then deserialize into [`SourceSpecRaw`].
    ///
    /// `serde_norway`'s `Value` deserializer only routes an *enum* input
    /// through its `Value::Tagged` variant — a plain single-key `Mapping`
    /// (the shape serde_json / yaml_to_json produces for an externally-
    /// tagged enum) is not accepted as an enum. So we normalise every
    /// incoming shape into a `Value::Tagged` before handing it to serde:
    ///
    /// - `Value::String(s)` (a bare word like `close`) → `Value::Tagged { tag:
    ///   s, value: Null }`, matching variant `s` with all fields defaulted.
    /// - `Value::Tagged` — forwarded verbatim (the YAML `!close` /
    ///   `!ema { ... }` form already has the right shape).
    /// - `Value::Mapping` with a single string key — rewritten as
    ///   `Value::Tagged { tag, value }`, so a serde_json → serde_norway::Value
    ///   bridge (which produces `Mapping`s for externally-tagged enums)
    ///   reaches the same code path.
    /// - Anything else (a `Number` for `Value(x)`, etc.) is forwarded verbatim
    ///   and serde_norway will report a helpful "unexpected type" error.
    ///
    /// Recursion into `Box<SourceSpec>` fields re-enters this same
    /// `TryFrom` — so a nested bare-word inside a tagged form is normalised
    /// on the way down.
    fn try_from(v: serde_norway::Value) -> Result<Self, Self::Error> {
        use serde_norway::value::{Tag, TaggedValue};

        // Unit-variant tags: their content stays as `Value::Null` because
        // serde's derived Deserialize expects unit content for a unit
        // variant. Every other variant is a struct with all-defaulted
        // fields, and a Null content there needs to be promoted to an
        // empty `Mapping` — serde's `deserialize_struct` accepts an empty
        // map (all fields default) but not `Null` (which errors with
        // "invalid type: unit value, expected struct variant"). The two
        // shapes both have to be normalised at the same layer because a
        // downstream `!pick` can appear as either an empty struct-variant
        // (`!pick {}` / `!pick`) or a filled one (`!pick { symbol: BTC }`).
        const UNIT_VARIANTS: &[&str] = &["entry", "peak", "trough"];

        let promote_null_for = |tag: &str, v: serde_norway::Value| match v {
            serde_norway::Value::Null if !UNIT_VARIANTS.contains(&tag) => {
                serde_norway::Value::Mapping(serde_norway::Mapping::new())
            }
            other => other,
        };

        let normalised = match v {
            serde_norway::Value::String(s) => {
                let value = if UNIT_VARIANTS.contains(&s.as_str()) {
                    serde_norway::Value::Null
                } else {
                    serde_norway::Value::Mapping(serde_norway::Mapping::new())
                };
                serde_norway::Value::Tagged(Box::new(TaggedValue {
                    tag: Tag::new(s),
                    value,
                }))
            }
            serde_norway::Value::Tagged(tagged) => {
                let TaggedValue { tag, value } = *tagged;
                let tag_name = tag.to_string();
                let name = tag_name.strip_prefix('!').unwrap_or(&tag_name);
                let value = promote_null_for(name, value);
                serde_norway::Value::Tagged(Box::new(TaggedValue { tag, value }))
            }
            serde_norway::Value::Mapping(m) if m.len() == 1 => {
                let (k, v) = m.into_iter().next().unwrap();
                match k {
                    serde_norway::Value::String(name) => {
                        let value = promote_null_for(&name, v);
                        serde_norway::Value::Tagged(Box::new(TaggedValue {
                            tag: Tag::new(name),
                            value,
                        }))
                    }
                    other => {
                        let mut m = serde_norway::Mapping::new();
                        m.insert(other, v);
                        serde_norway::Value::Mapping(m)
                    }
                }
            }
            other => other,
        };
        let raw: SourceSpecRaw =
            serde_norway::from_value(normalised).map_err(|e| e.to_string())?;
        Ok(raw.into())
    }
}


/// Resolve an optional cross-asset `source` spec into a concrete
/// atom-emitting source. When the spec is `None`, returns the implicit
/// empty-selector `Pick` (single-entry unpack); when `Some`, builds the
/// user's subtree (typically a `!pick { symbol, freq }`) and wraps as an
/// [`AsAtom`] view for the leaf's `T::of(source)` constructor.
fn atom_source_of(
    source: Option<&SourceSpec>,
    anchor: &Position,
    schema: &Arc<Schema>,
) -> AsAtom {
    match source {
        None => AsAtom::new(dyn_indicator::wrap(pick_root())),
        Some(s) => AsAtom::new(s.build(anchor, schema)),
    }
}

impl SourceSpec {
    /// Construct the live, runtime-typed source this spec describes as a
    /// `Box<dyn DynIndicator>`. `anchor` is the owning strategy's
    /// [`Position`], shared by any `entry` / `peak` / `trough` leaves in the
    /// tree; `schema` is the overlay [`Schema`] the atom stream carries, used
    /// by `!get { key }` to look up the column's declared [`OverlayType`] and
    /// dispatch to the right typed leaf.
    pub fn build(&self, anchor: &Position, schema: &Arc<Schema>) -> Box<dyn DynIndicator> {
        use SourceSpec::*;
        // Recursive-build shorthands: build `s`, view it as a library-typed
        // `Indicator<Input=Snapshot, Output=Real>` (or Candle) so it drops
        // into a concrete library constructor.
        let real = |s: &SourceSpec| AsReal::new(s.build(anchor, schema));
        let candle = |s: &SourceSpec| AsCandle::new(s.build(anchor, schema));
        // The `Pick`-shaped `source:` field on every atom-input leaf.
        let atom_src = |source: Option<&Box<SourceSpec>>| {
            atom_source_of(source.map(|b| &**b), anchor, schema)
        };

        match self {
            // --- atom-input leaves ---
            Close { source } => {
                let s = atom_src(source.as_ref());
                dyn_indicator::wrap(fugazi::indicators::Close::of(s))
            }
            High { source } => {
                let s = atom_src(source.as_ref());
                dyn_indicator::wrap(fugazi::indicators::High::of(s))
            }
            Low { source } => {
                let s = atom_src(source.as_ref());
                dyn_indicator::wrap(fugazi::indicators::Low::of(s))
            }
            Open { source } => {
                let s = atom_src(source.as_ref());
                dyn_indicator::wrap(fugazi::indicators::Open::of(s))
            }
            Volume { source } => {
                let s = atom_src(source.as_ref());
                dyn_indicator::wrap(fugazi::indicators::Volume::of(s))
            }
            Typical { source } => {
                let s = atom_src(source.as_ref());
                dyn_indicator::wrap(fugazi::indicators::Typical::of(s))
            }
            Median { source } => {
                let s = atom_src(source.as_ref());
                dyn_indicator::wrap(fugazi::indicators::Median::of(s))
            }
            Current { source } => {
                let s = atom_src(source.as_ref());
                dyn_indicator::wrap(fugazi::indicators::CurrentBar::of(s))
            }

            Pick { symbol, freq } => build_pick(symbol.as_deref(), freq.as_deref()),

            Value(x) => dyn_indicator::wrap(self::Value::<Snapshot<String>>::new(*x)),
            Entry => dyn_indicator::wrap(anchor.entry::<Snapshot<String>>()),
            Peak => dyn_indicator::wrap(anchor.peak::<Snapshot<String>>()),
            Trough => dyn_indicator::wrap(anchor.trough::<Snapshot<String>>()),

            Get { key, source } => {
                let s = atom_src(source.as_ref());
                build_get(schema, key, s)
            }

            Ema { source, period } => dyn_indicator::wrap(self::Ema::new(real(source), *period)),
            Sma { source, period } => dyn_indicator::wrap(self::Sma::new(real(source), *period)),
            Rma { source, period } => dyn_indicator::wrap(self::Rma::new(real(source), *period)),
            Wma { source, period } => dyn_indicator::wrap(self::Wma::new(real(source), *period)),
            Hma { source, period } => dyn_indicator::wrap(self::Hma::new(real(source), *period)),
            Rsi { source, period } => dyn_indicator::wrap(self::Rsi::new(real(source), *period)),
            StdDev { source, period } => {
                dyn_indicator::wrap(self::StdDev::new(real(source), *period))
            }
            Cci { source, period } => dyn_indicator::wrap(self::Cci::new(real(source), *period)),
            Stochastic { source, period } => {
                dyn_indicator::wrap(self::Stochastic::new(real(source), *period))
            }
            StochRsi {
                source,
                rsi_period,
                stoch_period,
            } => dyn_indicator::wrap(self::StochRsi::new(
                self::Rsi::new(real(source), *rsi_period),
                *stoch_period,
            )),

            MacdLine {
                source,
                fast,
                slow,
                signal,
            } => dyn_indicator::wrap(Component::new(
                Macd::new(real(source), *fast, *slow, *signal),
                |v: MacdValue| v.macd,
            )),
            MacdSignal {
                source,
                fast,
                slow,
                signal,
            } => dyn_indicator::wrap(Component::new(
                Macd::new(real(source), *fast, *slow, *signal),
                |v: MacdValue| v.signal,
            )),
            MacdHistogram {
                source,
                fast,
                slow,
                signal,
            } => dyn_indicator::wrap(Component::new(
                Macd::new(real(source), *fast, *slow, *signal),
                |v: MacdValue| v.histogram,
            )),

            BbUpper { source, period, k } => dyn_indicator::wrap(Component::new(
                Bollinger::new(real(source), *period, *k),
                |v: BollingerValue| v.upper,
            )),
            BbMiddle { source, period, k } => dyn_indicator::wrap(Component::new(
                Bollinger::new(real(source), *period, *k),
                |v: BollingerValue| v.middle,
            )),
            BbLower { source, period, k } => dyn_indicator::wrap(Component::new(
                Bollinger::new(real(source), *period, *k),
                |v: BollingerValue| v.lower,
            )),

            KeltnerUpper {
                source,
                candle_source,
                ema_period,
                atr_period,
                multiplier,
            } => dyn_indicator::wrap(Component::new(
                Keltner::new(
                    real(source),
                    candle(candle_source),
                    *ema_period,
                    *atr_period,
                    *multiplier,
                ),
                |v: KeltnerValue| v.upper,
            )),
            KeltnerMiddle {
                source,
                candle_source,
                ema_period,
                atr_period,
                multiplier,
            } => dyn_indicator::wrap(Component::new(
                Keltner::new(
                    real(source),
                    candle(candle_source),
                    *ema_period,
                    *atr_period,
                    *multiplier,
                ),
                |v: KeltnerValue| v.middle,
            )),
            KeltnerLower {
                source,
                candle_source,
                ema_period,
                atr_period,
                multiplier,
            } => dyn_indicator::wrap(Component::new(
                Keltner::new(
                    real(source),
                    candle(candle_source),
                    *ema_period,
                    *atr_period,
                    *multiplier,
                ),
                |v: KeltnerValue| v.lower,
            )),

            DonchianUpper { high, low, period } => dyn_indicator::wrap(Component::new(
                Donchian::new(real(high), real(low), *period),
                |v: DonchianValue| v.upper,
            )),
            DonchianMiddle { high, low, period } => dyn_indicator::wrap(Component::new(
                Donchian::new(real(high), real(low), *period),
                |v: DonchianValue| v.middle,
            )),
            DonchianLower { high, low, period } => dyn_indicator::wrap(Component::new(
                Donchian::new(real(high), real(low), *period),
                |v: DonchianValue| v.lower,
            )),

            Adx { source, period } => dyn_indicator::wrap(Component::new(
                self::Adx::new(candle(source), *period),
                |v: AdxValue| v.adx,
            )),
            PlusDi { source, period } => dyn_indicator::wrap(Component::new(
                self::Adx::new(candle(source), *period),
                |v: AdxValue| v.plus_di,
            )),
            MinusDi { source, period } => dyn_indicator::wrap(Component::new(
                self::Adx::new(candle(source), *period),
                |v: AdxValue| v.minus_di,
            )),
            DmiPlusDi { source, period } => dyn_indicator::wrap(Component::new(
                self::Dmi::new(candle(source), *period),
                |v: DmiValue| v.plus_di,
            )),
            DmiMinusDi { source, period } => dyn_indicator::wrap(Component::new(
                self::Dmi::new(candle(source), *period),
                |v: DmiValue| v.minus_di,
            )),

            AroonUp { source, period } => dyn_indicator::wrap(Component::new(
                self::Aroon::new(candle(source), *period),
                |v: AroonValue| v.up,
            )),
            AroonDown { source, period } => dyn_indicator::wrap(Component::new(
                self::Aroon::new(candle(source), *period),
                |v: AroonValue| v.down,
            )),
            AroonOscillator { source, period } => dyn_indicator::wrap(Component::new(
                self::Aroon::new(candle(source), *period),
                |v: AroonValue| v.oscillator,
            )),

            Atr { source, period } => dyn_indicator::wrap(self::Atr::new(candle(source), *period)),
            Mfi { source, period } => dyn_indicator::wrap(self::Mfi::new(candle(source), *period)),
            WilliamsR { source, period } => {
                dyn_indicator::wrap(self::WilliamsR::new(candle(source), *period))
            }
            Obv { source } => dyn_indicator::wrap(self::Obv::new(candle(source))),
            Vwap { source } => dyn_indicator::wrap(self::Vwap::new(candle(source))),
            Ad { source } => dyn_indicator::wrap(self::Ad::new(candle(source))),
            TrueRange { source } => dyn_indicator::wrap(self::TrueRange::new(candle(source))),
            Sar { source, step, max } => {
                dyn_indicator::wrap(self::Sar::new(candle(source), *step, *max))
            }

            VolTarget {
                target,
                window,
                bars_per_year,
            } => dyn_indicator::wrap(fugazi::indicators::sizing::vol_target::<String>(
                *target,
                *window,
                *bars_per_year,
            )),
            AtrRisk {
                risk_frac,
                period,
                atr_multiple,
            } => dyn_indicator::wrap(fugazi::indicators::sizing::atr_risk::<String>(
                *risk_frac,
                *period,
                *atr_multiple,
            )),

            Add { lhs, rhs } => dyn_indicator::wrap(real(lhs).add(real(rhs))),
            Sub { lhs, rhs } => dyn_indicator::wrap(real(lhs).sub(real(rhs))),
            Mul { lhs, rhs } => dyn_indicator::wrap(real(lhs).mul(real(rhs))),
            Div { lhs, rhs } => dyn_indicator::wrap(real(lhs).div(real(rhs))),
            Lag { source, periods } => dyn_indicator::wrap(real(source).lag(*periods)),
            Diff { source, periods } => dyn_indicator::wrap(real(source).diff(*periods)),
            Ratio { source, periods } => dyn_indicator::wrap(real(source).ratio(*periods)),
            Roc { source, periods } => dyn_indicator::wrap(real(source).roc(*periods)),
            RollingMax { source, period } => {
                dyn_indicator::wrap(real(source).rolling_max(*period))
            }
            RollingMin { source, period } => {
                dyn_indicator::wrap(real(source).rolling_min(*period))
            }
            Log { source, base } => dyn_indicator::wrap(self::Log::new(real(source), *base)),
            Latch { source } => {
                let inner = AsReal::new(source.build(anchor, schema));
                dyn_indicator::wrap(self::Latch::new(inner))
            }
            Resample {
                every,
                inner,
                source,
            } => {
                assert!(*every > 0, "resample every must be greater than zero");
                let candle_src = candle(source);
                let resample_dyn = dyn_indicator::wrap(self::Resample::new(candle_src, *every));
                let inner_dyn = inner.build(anchor, schema);
                dyn_indicator::chain(resample_dyn, inner_dyn)
            }
            Unstable { source } => dyn_indicator::unstable_wrap(source.build(anchor, schema)),

            Year { source } => {
                let s = atom_src(source.as_ref());
                dyn_indicator::wrap(fugazi::indicators::Year::of(s))
            }
            Month { source } => {
                let s = atom_src(source.as_ref());
                dyn_indicator::wrap(fugazi::indicators::Month::of(s))
            }
            Day { source } => {
                let s = atom_src(source.as_ref());
                dyn_indicator::wrap(fugazi::indicators::Day::of(s))
            }
            Hour { source } => {
                let s = atom_src(source.as_ref());
                dyn_indicator::wrap(fugazi::indicators::Hour::of(s))
            }
            Minute { source } => {
                let s = atom_src(source.as_ref());
                dyn_indicator::wrap(fugazi::indicators::Minute::of(s))
            }
            Second { source } => {
                let s = atom_src(source.as_ref());
                dyn_indicator::wrap(fugazi::indicators::Second::of(s))
            }
            DayOfWeek { source } => {
                let s = atom_src(source.as_ref());
                dyn_indicator::wrap(fugazi::indicators::DayOfWeek::of(s))
            }
            DayOfYear { source } => {
                let s = atom_src(source.as_ref());
                dyn_indicator::wrap(fugazi::indicators::DayOfYear::of(s))
            }
            WeekOfYear { source } => {
                let s = atom_src(source.as_ref());
                dyn_indicator::wrap(fugazi::indicators::WeekOfYear::of(s))
            }
            Quarter { source } => {
                let s = atom_src(source.as_ref());
                dyn_indicator::wrap(fugazi::indicators::Quarter::of(s))
            }
            UnixSeconds { source } => {
                let s = atom_src(source.as_ref());
                dyn_indicator::wrap(fugazi::indicators::UnixSeconds::of(s))
            }
            UnixMillis { source } => {
                let s = atom_src(source.as_ref());
                dyn_indicator::wrap(fugazi::indicators::UnixMillis::of(s))
            }
            Time { source } => {
                let s = atom_src(source.as_ref());
                dyn_indicator::wrap(fugazi::indicators::CurrentTime::of(s))
            }
        }
    }
}

/// Build a `!pick { symbol, freq }` leaf. Both fields are optional; the
/// empty selector (`!pick {}`) behaves as the single-entry sole-atom unpack
/// every atom-input leaf uses by default. A `freq` string is parsed via
/// [`Frequency::from_str`] (the `N<unit>` alphabet: `1m`/`4h`/`1d`/`1w`/`1M`);
/// a parse failure panics with the offending string included.
fn build_pick(symbol: Option<&str>, freq: Option<&str>) -> Box<dyn DynIndicator> {
    let sym = symbol.map(String::from);
    let f = freq.map(|s| {
        Frequency::from_str(s)
            .unwrap_or_else(|e| panic!("!pick {{ freq: {s:?} }}: invalid frequency: {e}"))
    });
    let selector = Selector::<String> {
        symbol: sym,
        freq: f,
    };
    if selector.is_empty() {
        dyn_indicator::wrap(Pick::<String>::new())
    } else {
        dyn_indicator::wrap(Pick::<String>::matching(selector))
    }
}

/// Build a `!get { key, source }` leaf: look up the column's declared
/// [`OverlayType`] in `schema` and dispatch to the matching typed
/// [`GetReal`] / [`GetBool`] / [`GetStr`] leaf, rooted on the caller-provided
/// atom source (typically the implicit `Pick::new()` unpack, or an explicit
/// `!pick { symbol, freq }` for cross-asset overlays).
///
/// Panics with a helpful message if `key` isn't registered — the message
/// lists the schema's registered keys so a typo is easy to spot. The message
/// distinguishes the empty-schema case ("no overlay side channel — feed
/// `--series` or `csv:` data with additional columns to attach overlays")
/// from the non-empty case ("registered: a, b, c").
fn build_get(schema: &Arc<Schema>, key: &str, source: AsAtom) -> Box<dyn DynIndicator> {
    match schema.type_of_key(key) {
        Some(OverlayType::Real) => dyn_indicator::wrap(GetReal::of(schema, key, source)),
        Some(OverlayType::Bool) => dyn_indicator::wrap(GetBool::of(schema, key, source)),
        Some(OverlayType::Str) => dyn_indicator::wrap(GetStr::of(schema, key, source)),
        None => {
            let registered: Vec<&str> = schema.keys().collect();
            if registered.is_empty() {
                panic!(
                    "!get {{ key: {key:?} }}: no overlay side channel is bound \
                     — feed `--series` data or a `csv:` source that carries \
                     additional (non-OHLCV) columns to attach overlays",
                );
            } else {
                panic!(
                    "!get {{ key: {key:?} }}: overlay column not registered. \
                     Registered columns: {}",
                    registered.join(", "),
                );
            }
        }
    }
}
