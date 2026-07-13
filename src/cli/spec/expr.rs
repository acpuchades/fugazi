//! YAML-deserializable [`ExprSpec`] — the value-producing expression layer.
//!
//! Every YAML tag that produces a value (numeric or otherwise —
//! `!close`/`!ema`/`!current`/`!pick`/`!time`/`!get` etc.) is a variant of
//! this enum. The twin [`SignalSpec`](super::signal::SignalSpec) covers the
//! bool-valued predicates. Together they form the CLI's composable
//! expression surface: a `SideSpec::stop_loss` is an `ExprSpec` (an
//! expression producing a Real level); a `SideSpec::enter` is a
//! `SignalSpec` (an expression producing a Bool signal).
//!
//! Split out of `spec/mod.rs`; the module lives at `crate::spec::expr` and
//! the type is re-exported at `crate::spec::ExprSpec` via `mod.rs`.

use std::sync::Arc;

use serde::Deserialize;

// Field / calendar / current-bar / current-time leaves are referenced through
// their full `fugazi::indicators::` paths inside the `ExprSpec::build`
// match arms — the source-spec variants share those names (Close, High, Year,
// …) as enum-variant identifiers, so a bare `Close::of(...)` would try to
// resolve on the enum variant. The `Pick` root is the one exception because
// it isn't a `ExprSpec` variant.
use fugazi::indicators::{
    Ad, Adx, AdxValue, Aroon, AroonValue, Atr, Bollinger, BollingerValue, Book, Cci, Component,
    Correlation, Dmi, DmiValue, Donchian, DonchianValue, Ema, GarmanKlass, GetBool, GetReal, GetStr,
    Hma, IfElse, Keltner, KeltnerValue, Kurtosis, Latch, Log, Macd, MacdValue, Mfi, Obv, Parkinson,
    Pick, Position, Resample, Rma, RogersSatchell, Rsi, Sar, Skewness, Sma, StdDev, StochRsi,
    Stochastic, TrueRange, Value, ValueStr, Vwap, WilliamsR, Wma, ZScore,
};
use fugazi::prelude::*;
use fugazi::types::Snapshot;

use super::signal::SignalSpec;
use crate::dyn_indicator::{self, AsAtom, AsBool, AsCandle, AsReal, DynIndicator};

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

pub(super) fn default_source() -> Box<ExprSpec> {
    Box::new(ExprSpec::Close { source: None })
}
pub(super) fn default_high() -> Box<ExprSpec> {
    Box::new(ExprSpec::High { source: None })
}
pub(super) fn default_low() -> Box<ExprSpec> {
    Box::new(ExprSpec::Low { source: None })
}
/// Default candle source for bar indicators — the current bar itself.
pub(super) fn default_bar_source() -> Box<ExprSpec> {
    Box::new(ExprSpec::Current { source: None })
}

/// Default base for `!log`: natural log (`e`).
pub(super) fn default_log_base() -> Real {
    std::f64::consts::E
}

/// The payload of [`ExprSpec::Value`] — a constant leaf, either numeric or
/// string-valued.
///
/// A YAML number builds a [`Value`] (`Real` output, the operand of every
/// arithmetic op and comparison); a YAML string builds a
/// [`ValueStr`] (`Arc<str>` output, the operand of `!str_eq` / `!str_ne`
/// against a `Str` overlay column read by `!get`):
///
/// ```yaml
/// !gt      { lhs: !rsi { period: 14 }, rhs: !value 70 }        # Real
/// !str_ne  { lhs: !get { key: regime }, rhs: !value bear }     # Str
/// ```
///
/// Quoting decides the type when the two would collide: `!value 70` is the
/// number, `!value "70"` the string. Deserializes through a
/// [`serde_norway::Value`] bridge (rather than `#[serde(untagged)]`) so a
/// wrong-typed literal reports what `!value` accepts instead of the
/// "did not match any variant" untagged error.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(try_from = "serde_norway::Value")]
pub enum ValueLit {
    Real(Real),
    Str(String),
}

impl TryFrom<serde_norway::Value> for ValueLit {
    type Error = String;

    fn try_from(v: serde_norway::Value) -> Result<Self, Self::Error> {
        match v {
            serde_norway::Value::Number(n) => n
                .as_f64()
                .map(ValueLit::Real)
                .ok_or_else(|| format!("!value: {n} is not a finite number")),
            serde_norway::Value::String(s) => Ok(ValueLit::Str(s)),
            other => Err(format!(
                "!value takes a number (a constant scalar) or a string (a \
                 constant string, for !str_eq / !str_ne), got {other:?}"
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Real-valued sources
// ---------------------------------------------------------------------------

/// A real-valued source over a candle stream — the YAML form of any
/// `Indicator<Input = Candle, Output = Real>`.
///
/// Every atom-input leaf (`!close`, `!high`, …, all calendar accessors, and
/// `!get`) carries a **defaulted optional `source: Option<Box<ExprSpec>>`**
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
/// tagged shapes into the map shape [`ExprSpecRaw`] expects, and
/// [`ExprSpecRaw`] carries the derived externally-tagged deserialization
/// logic.
#[derive(Debug, Clone, Deserialize)]
#[serde(try_from = "serde_norway::Value")]
pub enum ExprSpec {
    // --- atom-input leaves (candle fields) ---
    Close {
        #[serde(default)]
        source: Option<Box<ExprSpec>>,
    },
    High {
        #[serde(default)]
        source: Option<Box<ExprSpec>>,
    },
    Low {
        #[serde(default)]
        source: Option<Box<ExprSpec>>,
    },
    Open {
        #[serde(default)]
        source: Option<Box<ExprSpec>>,
    },
    Volume {
        #[serde(default)]
        source: Option<Box<ExprSpec>>,
    },
    Typical {
        #[serde(default)]
        source: Option<Box<ExprSpec>>,
    },
    Median {
        #[serde(default)]
        source: Option<Box<ExprSpec>>,
    },
    /// The current bar itself — the whole [`Candle`], not a scalar. The default
    /// bar source of every bar-consuming indicator (`!atr`, `!obv`, `!adx`, …);
    /// wrap in [`ExprSpec::Resample`] to lift a downstream bar indicator
    /// onto a higher timeframe.
    Current {
        #[serde(default)]
        source: Option<Box<ExprSpec>>,
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

    /// A constant value — a number (`!value 70`, a `Real` source) or a string
    /// (`!value bull`, a `Str` source for `!str_eq` / `!str_ne`). See
    /// [`ValueLit`].
    Value(ValueLit),

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
        source: Option<Box<ExprSpec>>,
    },

    // --- price-series indicators (a source + parameters) ---
    Ema {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        period: usize,
    },
    Sma {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        period: usize,
    },
    Rma {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        period: usize,
    },
    Wma {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        period: usize,
    },
    Hma {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        period: usize,
    },
    Rsi {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        period: usize,
    },
    #[serde(rename = "stddev")]
    StdDev {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        period: usize,
    },
    Skewness {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        period: usize,
    },
    Kurtosis {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        period: usize,
    },
    #[serde(rename = "zscore")]
    ZScore {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        period: usize,
    },
    /// Rolling Pearson correlation between two Real sources. Both operands are
    /// required — there is no single natural default for a two-source stat.
    Correlation {
        lhs: Box<ExprSpec>,
        rhs: Box<ExprSpec>,
        period: usize,
    },
    Cci {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        period: usize,
    },
    Stochastic {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        period: usize,
    },
    StochRsi {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        rsi_period: usize,
        stoch_period: usize,
    },

    // --- multi-output indicators, one variant per component ---
    MacdLine {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        fast: usize,
        slow: usize,
        signal: usize,
    },
    MacdSignal {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        fast: usize,
        slow: usize,
        signal: usize,
    },
    MacdHistogram {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        fast: usize,
        slow: usize,
        signal: usize,
    },
    BbUpper {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        period: usize,
        k: Real,
    },
    BbMiddle {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        period: usize,
        k: Real,
    },
    BbLower {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        period: usize,
        k: Real,
    },
    KeltnerUpper {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        #[serde(default = "default_bar_source")]
        candle_source: Box<ExprSpec>,
        ema_period: usize,
        atr_period: usize,
        multiplier: Real,
    },
    KeltnerMiddle {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        #[serde(default = "default_bar_source")]
        candle_source: Box<ExprSpec>,
        ema_period: usize,
        atr_period: usize,
        multiplier: Real,
    },
    KeltnerLower {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        #[serde(default = "default_bar_source")]
        candle_source: Box<ExprSpec>,
        ema_period: usize,
        atr_period: usize,
        multiplier: Real,
    },
    DonchianUpper {
        #[serde(default = "default_high")]
        high: Box<ExprSpec>,
        #[serde(default = "default_low")]
        low: Box<ExprSpec>,
        period: usize,
    },
    DonchianMiddle {
        #[serde(default = "default_high")]
        high: Box<ExprSpec>,
        #[serde(default = "default_low")]
        low: Box<ExprSpec>,
        period: usize,
    },
    DonchianLower {
        #[serde(default = "default_high")]
        high: Box<ExprSpec>,
        #[serde(default = "default_low")]
        low: Box<ExprSpec>,
        period: usize,
    },
    Adx {
        #[serde(default = "default_bar_source")]
        source: Box<ExprSpec>,
        period: usize,
    },
    PlusDi {
        #[serde(default = "default_bar_source")]
        source: Box<ExprSpec>,
        period: usize,
    },
    MinusDi {
        #[serde(default = "default_bar_source")]
        source: Box<ExprSpec>,
        period: usize,
    },
    DmiPlusDi {
        #[serde(default = "default_bar_source")]
        source: Box<ExprSpec>,
        period: usize,
    },
    DmiMinusDi {
        #[serde(default = "default_bar_source")]
        source: Box<ExprSpec>,
        period: usize,
    },
    AroonUp {
        #[serde(default = "default_bar_source")]
        source: Box<ExprSpec>,
        period: usize,
    },
    AroonDown {
        #[serde(default = "default_bar_source")]
        source: Box<ExprSpec>,
        period: usize,
    },
    AroonOscillator {
        #[serde(default = "default_bar_source")]
        source: Box<ExprSpec>,
        period: usize,
    },

    // --- single-output bar indicators ---
    Atr {
        #[serde(default = "default_bar_source")]
        source: Box<ExprSpec>,
        period: usize,
    },
    /// Parkinson high/low range volatility estimator over `period`.
    Parkinson {
        #[serde(default = "default_bar_source")]
        source: Box<ExprSpec>,
        period: usize,
    },
    /// Garman–Klass OHLC volatility estimator over `period`.
    GarmanKlass {
        #[serde(default = "default_bar_source")]
        source: Box<ExprSpec>,
        period: usize,
    },
    /// Rogers–Satchell drift-independent OHLC volatility estimator over `period`.
    RogersSatchell {
        #[serde(default = "default_bar_source")]
        source: Box<ExprSpec>,
        period: usize,
    },
    Mfi {
        #[serde(default = "default_bar_source")]
        source: Box<ExprSpec>,
        period: usize,
    },
    WilliamsR {
        #[serde(default = "default_bar_source")]
        source: Box<ExprSpec>,
        period: usize,
    },
    Obv {
        #[serde(default = "default_bar_source")]
        source: Box<ExprSpec>,
    },
    Vwap {
        #[serde(default = "default_bar_source")]
        source: Box<ExprSpec>,
    },
    Ad {
        #[serde(default = "default_bar_source")]
        source: Box<ExprSpec>,
    },
    TrueRange {
        #[serde(default = "default_bar_source")]
        source: Box<ExprSpec>,
    },
    Sar {
        #[serde(default = "default_bar_source")]
        source: Box<ExprSpec>,
        step: Real,
        max: Real,
    },

    // --- sizing helpers (real-valued, single-series; read the strategy's
    // own asset via the implicit empty-selector `Pick`). Meant for the
    // `sizing:` slot on `SingleStrategySpec` / `PairsStrategySpec`, but usable
    // anywhere a real-valued source fits. The book-anchored ones
    // (`DrawdownThrottle`, `EquityVolTarget`, `FractionalKelly`) additionally
    // require the strategy to own a `Book` — `SingleStrategySpec` does;
    // `PairsStrategySpec` does not (they'll emit `None` there).
    /// Equal-weight sizing — a constant `1.0 / n_legs`. The one-line
    /// helper the common basket case reaches for: `sizing: !equal_weight
    /// 6` on a 3-long / 3-short basket yields 1/6 per leg = 100% gross
    /// exposure. Doesn't depend on the symbol, so no `!arg SYM` needed.
    /// See [`fugazi::indicators::sizing::equal_weight`].
    EqualWeight(usize),
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
    /// Drawdown-throttled sizing — `max(0, min(1, 1 + book.drawdown() /
    /// max_drawdown))`. Reads the strategy's [`Book`] anchor. See
    /// [`fugazi::indicators::sizing::drawdown_throttle`].
    DrawdownThrottle { max_drawdown: Real },
    /// Realized-vol targeting on the strategy's own equity return series
    /// — `target / (stddev(book.return_per_bar, window) *
    /// sqrt(bars_per_year))`. Reads the strategy's [`Book`] anchor. See
    /// [`fugazi::indicators::sizing::equity_vol_target`].
    EquityVolTarget {
        target: Real,
        window: usize,
        bars_per_year: Real,
    },
    /// Fractional Kelly over the last `window` closed-trade returns —
    /// `kelly_fraction * mean / variance`, clamped to `>= 0`. Reads the
    /// strategy's [`Book`] anchor. See
    /// [`fugazi::indicators::sizing::fractional_kelly`].
    FractionalKelly {
        kelly_fraction: Real,
        window: usize,
    },

    // --- transform operators ---
    Add {
        lhs: Box<ExprSpec>,
        rhs: Box<ExprSpec>,
    },
    Sub {
        lhs: Box<ExprSpec>,
        rhs: Box<ExprSpec>,
    },
    Mul {
        lhs: Box<ExprSpec>,
        rhs: Box<ExprSpec>,
    },
    Div {
        lhs: Box<ExprSpec>,
        rhs: Box<ExprSpec>,
    },
    /// Three-source ternary: reads `cond` (a bool signal), emits
    /// `if_true`'s value when `cond` is true, `if_false`'s when false, and
    /// `None` when `cond` is `None`. All three sources are advanced every
    /// bar so a branch that doesn't fire this bar keeps warming up in the
    /// background. Warm-up is the max of the three; the ternary reports
    /// `None` until every source has warmed. See
    /// [`fugazi::indicators::IfElse`].
    IfElse {
        cond: Box<SignalSpec>,
        if_true: Box<ExprSpec>,
        if_false: Box<ExprSpec>,
    },
    Lag {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        periods: usize,
    },
    Diff {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        periods: usize,
    },
    Ratio {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        periods: usize,
    },
    Roc {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        periods: usize,
    },
    RollingMax {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        period: usize,
    },
    RollingMin {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        period: usize,
    },
    /// Logarithm of `source` in `base` (defaults to natural log, `e`).
    /// Emits `None` on samples where the source's output is non-positive.
    Log {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        #[serde(default = "default_log_base")]
        base: Real,
    },
    /// Holds the most recent `Some` output of `source`, re-emitting it on
    /// ticks where `source` returns `None`. Wrap the outermost recursive
    /// smoother of a resampled pipeline so per-base-tick consumers see the
    /// finished higher-timeframe value between boundaries — see
    /// [`fugazi::indicators::Latch`].
    Latch { source: Box<ExprSpec> },
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
    /// chain in [`Latch`](ExprSpec::Latch) so per-base-tick reads see the
    /// finished value between boundaries.
    Resample {
        every: usize,
        inner: Box<ExprSpec>,
        #[serde(default = "default_bar_source")]
        source: Box<ExprSpec>,
    },
    /// Passthrough wrapper that reports `unstable_period() = 0`. The output
    /// and warm-up of `source` are unchanged; the strategy-readiness gate
    /// (which counts up to `stable_period()`) no longer waits for this
    /// subtree's IIR settling tail. The explicit opt-out to the "wait for
    /// every source to be past its unstable tail" safe default; see
    /// [`fugazi::indicators::Unstable`].
    Unstable { source: Box<ExprSpec> },

    // --- calendar accessors (read `atom.time`, emit Real; None when time is
    // absent). Each takes an optional `source` for cross-asset use — the
    // bare form (`!year`) is the default single-series shortcut,
    // `!year { source: !pick { ... } }` reads the picked asset's time.
    /// The Gregorian year (e.g. `2024.0`).
    Year {
        #[serde(default)]
        source: Option<Box<ExprSpec>>,
    },
    /// The Gregorian month, `1.0` (Jan) through `12.0` (Dec).
    Month {
        #[serde(default)]
        source: Option<Box<ExprSpec>>,
    },
    /// The day of the month, `1.0` through `31.0`.
    Day {
        #[serde(default)]
        source: Option<Box<ExprSpec>>,
    },
    /// The hour of the day (UTC), `0.0` through `23.0`.
    Hour {
        #[serde(default)]
        source: Option<Box<ExprSpec>>,
    },
    /// The minute of the hour, `0.0` through `59.0`.
    Minute {
        #[serde(default)]
        source: Option<Box<ExprSpec>>,
    },
    /// The second of the minute, `0.0` through `59.0`.
    Second {
        #[serde(default)]
        source: Option<Box<ExprSpec>>,
    },
    /// ISO 8601 weekday, `1.0` (Monday) through `7.0` (Sunday).
    DayOfWeek {
        #[serde(default)]
        source: Option<Box<ExprSpec>>,
    },
    /// Day of the year, `1.0` through `366.0`.
    DayOfYear {
        #[serde(default)]
        source: Option<Box<ExprSpec>>,
    },
    /// ISO 8601 week of the year, `1.0` through `53.0`.
    WeekOfYear {
        #[serde(default)]
        source: Option<Box<ExprSpec>>,
    },
    /// Calendar quarter, `1.0` through `4.0`.
    Quarter {
        #[serde(default)]
        source: Option<Box<ExprSpec>>,
    },
    /// Unix seconds since the epoch (as a Real).
    UnixSeconds {
        #[serde(default)]
        source: Option<Box<ExprSpec>>,
    },
    /// Unix milliseconds since the epoch (as a Real).
    UnixMillis {
        #[serde(default)]
        source: Option<Box<ExprSpec>>,
    },
    /// The raw bar-open [`Timestamp`] payload (yields
    /// `DynType::Time`, not a scalar). The `Timestamp` twin of
    /// [`ExprSpec::Current`].
    Time {
        #[serde(default)]
        source: Option<Box<ExprSpec>>,
    },
}

// Mirror enum: identical shape as ExprSpec but with derived Deserialize —
// used inside TryFrom<serde_norway::Value> to run the standard externally-
// tagged deserialization once bare-string / tagged shapes are normalised.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
enum ExprSpecRaw {

    // --- atom-input leaves (candle fields) ---
    Close {
        #[serde(default)]
        source: Option<Box<ExprSpec>>,
    },
    High {
        #[serde(default)]
        source: Option<Box<ExprSpec>>,
    },
    Low {
        #[serde(default)]
        source: Option<Box<ExprSpec>>,
    },
    Open {
        #[serde(default)]
        source: Option<Box<ExprSpec>>,
    },
    Volume {
        #[serde(default)]
        source: Option<Box<ExprSpec>>,
    },
    Typical {
        #[serde(default)]
        source: Option<Box<ExprSpec>>,
    },
    Median {
        #[serde(default)]
        source: Option<Box<ExprSpec>>,
    },
    /// The current bar itself — the whole [`Candle`], not a scalar. The default
    /// bar source of every bar-consuming indicator (`!atr`, `!obv`, `!adx`, …);
    /// wrap in [`ExprSpec::Resample`] to lift a downstream bar indicator
    /// onto a higher timeframe.
    Current {
        #[serde(default)]
        source: Option<Box<ExprSpec>>,
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

    /// A constant value — a number (`!value 70`, a `Real` source) or a string
    /// (`!value bull`, a `Str` source for `!str_eq` / `!str_ne`). See
    /// [`ValueLit`].
    Value(ValueLit),

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
        source: Option<Box<ExprSpec>>,
    },

    // --- price-series indicators (a source + parameters) ---
    Ema {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        period: usize,
    },
    Sma {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        period: usize,
    },
    Rma {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        period: usize,
    },
    Wma {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        period: usize,
    },
    Hma {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        period: usize,
    },
    Rsi {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        period: usize,
    },
    #[serde(rename = "stddev")]
    StdDev {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        period: usize,
    },
    Skewness {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        period: usize,
    },
    Kurtosis {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        period: usize,
    },
    #[serde(rename = "zscore")]
    ZScore {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        period: usize,
    },
    /// Rolling Pearson correlation between two Real sources. Both operands are
    /// required — there is no single natural default for a two-source stat.
    Correlation {
        lhs: Box<ExprSpec>,
        rhs: Box<ExprSpec>,
        period: usize,
    },
    Cci {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        period: usize,
    },
    Stochastic {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        period: usize,
    },
    StochRsi {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        rsi_period: usize,
        stoch_period: usize,
    },

    // --- multi-output indicators, one variant per component ---
    MacdLine {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        fast: usize,
        slow: usize,
        signal: usize,
    },
    MacdSignal {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        fast: usize,
        slow: usize,
        signal: usize,
    },
    MacdHistogram {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        fast: usize,
        slow: usize,
        signal: usize,
    },
    BbUpper {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        period: usize,
        k: Real,
    },
    BbMiddle {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        period: usize,
        k: Real,
    },
    BbLower {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        period: usize,
        k: Real,
    },
    KeltnerUpper {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        #[serde(default = "default_bar_source")]
        candle_source: Box<ExprSpec>,
        ema_period: usize,
        atr_period: usize,
        multiplier: Real,
    },
    KeltnerMiddle {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        #[serde(default = "default_bar_source")]
        candle_source: Box<ExprSpec>,
        ema_period: usize,
        atr_period: usize,
        multiplier: Real,
    },
    KeltnerLower {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        #[serde(default = "default_bar_source")]
        candle_source: Box<ExprSpec>,
        ema_period: usize,
        atr_period: usize,
        multiplier: Real,
    },
    DonchianUpper {
        #[serde(default = "default_high")]
        high: Box<ExprSpec>,
        #[serde(default = "default_low")]
        low: Box<ExprSpec>,
        period: usize,
    },
    DonchianMiddle {
        #[serde(default = "default_high")]
        high: Box<ExprSpec>,
        #[serde(default = "default_low")]
        low: Box<ExprSpec>,
        period: usize,
    },
    DonchianLower {
        #[serde(default = "default_high")]
        high: Box<ExprSpec>,
        #[serde(default = "default_low")]
        low: Box<ExprSpec>,
        period: usize,
    },
    Adx {
        #[serde(default = "default_bar_source")]
        source: Box<ExprSpec>,
        period: usize,
    },
    PlusDi {
        #[serde(default = "default_bar_source")]
        source: Box<ExprSpec>,
        period: usize,
    },
    MinusDi {
        #[serde(default = "default_bar_source")]
        source: Box<ExprSpec>,
        period: usize,
    },
    DmiPlusDi {
        #[serde(default = "default_bar_source")]
        source: Box<ExprSpec>,
        period: usize,
    },
    DmiMinusDi {
        #[serde(default = "default_bar_source")]
        source: Box<ExprSpec>,
        period: usize,
    },
    AroonUp {
        #[serde(default = "default_bar_source")]
        source: Box<ExprSpec>,
        period: usize,
    },
    AroonDown {
        #[serde(default = "default_bar_source")]
        source: Box<ExprSpec>,
        period: usize,
    },
    AroonOscillator {
        #[serde(default = "default_bar_source")]
        source: Box<ExprSpec>,
        period: usize,
    },

    // --- single-output bar indicators ---
    Atr {
        #[serde(default = "default_bar_source")]
        source: Box<ExprSpec>,
        period: usize,
    },
    /// Parkinson high/low range volatility estimator over `period`.
    Parkinson {
        #[serde(default = "default_bar_source")]
        source: Box<ExprSpec>,
        period: usize,
    },
    /// Garman–Klass OHLC volatility estimator over `period`.
    GarmanKlass {
        #[serde(default = "default_bar_source")]
        source: Box<ExprSpec>,
        period: usize,
    },
    /// Rogers–Satchell drift-independent OHLC volatility estimator over `period`.
    RogersSatchell {
        #[serde(default = "default_bar_source")]
        source: Box<ExprSpec>,
        period: usize,
    },
    Mfi {
        #[serde(default = "default_bar_source")]
        source: Box<ExprSpec>,
        period: usize,
    },
    WilliamsR {
        #[serde(default = "default_bar_source")]
        source: Box<ExprSpec>,
        period: usize,
    },
    Obv {
        #[serde(default = "default_bar_source")]
        source: Box<ExprSpec>,
    },
    Vwap {
        #[serde(default = "default_bar_source")]
        source: Box<ExprSpec>,
    },
    Ad {
        #[serde(default = "default_bar_source")]
        source: Box<ExprSpec>,
    },
    TrueRange {
        #[serde(default = "default_bar_source")]
        source: Box<ExprSpec>,
    },
    Sar {
        #[serde(default = "default_bar_source")]
        source: Box<ExprSpec>,
        step: Real,
        max: Real,
    },

    // --- sizing helpers (real-valued, single-series; read the strategy's
    // own asset via the implicit empty-selector `Pick`). Meant for the
    // `sizing:` slot on `SingleStrategySpec` / `PairsStrategySpec`, but usable
    // anywhere a real-valued source fits. The book-anchored ones
    // (`DrawdownThrottle`, `EquityVolTarget`, `FractionalKelly`) additionally
    // require the strategy to own a `Book` — `SingleStrategySpec` does;
    // `PairsStrategySpec` does not (they'll emit `None` there).
    /// Equal-weight sizing — a constant `1.0 / n_legs`. The one-line
    /// helper the common basket case reaches for: `sizing: !equal_weight
    /// 6` on a 3-long / 3-short basket yields 1/6 per leg = 100% gross
    /// exposure. Doesn't depend on the symbol, so no `!arg SYM` needed.
    /// See [`fugazi::indicators::sizing::equal_weight`].
    EqualWeight(usize),
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
    /// Drawdown-throttled sizing — `max(0, min(1, 1 + book.drawdown() /
    /// max_drawdown))`. Reads the strategy's [`Book`] anchor. See
    /// [`fugazi::indicators::sizing::drawdown_throttle`].
    DrawdownThrottle { max_drawdown: Real },
    /// Realized-vol targeting on the strategy's own equity return series
    /// — `target / (stddev(book.return_per_bar, window) *
    /// sqrt(bars_per_year))`. Reads the strategy's [`Book`] anchor. See
    /// [`fugazi::indicators::sizing::equity_vol_target`].
    EquityVolTarget {
        target: Real,
        window: usize,
        bars_per_year: Real,
    },
    /// Fractional Kelly over the last `window` closed-trade returns —
    /// `kelly_fraction * mean / variance`, clamped to `>= 0`. Reads the
    /// strategy's [`Book`] anchor. See
    /// [`fugazi::indicators::sizing::fractional_kelly`].
    FractionalKelly {
        kelly_fraction: Real,
        window: usize,
    },

    // --- transform operators ---
    Add {
        lhs: Box<ExprSpec>,
        rhs: Box<ExprSpec>,
    },
    Sub {
        lhs: Box<ExprSpec>,
        rhs: Box<ExprSpec>,
    },
    Mul {
        lhs: Box<ExprSpec>,
        rhs: Box<ExprSpec>,
    },
    Div {
        lhs: Box<ExprSpec>,
        rhs: Box<ExprSpec>,
    },
    /// Three-source ternary: reads `cond` (a bool signal), emits
    /// `if_true`'s value when `cond` is true, `if_false`'s when false, and
    /// `None` when `cond` is `None`. All three sources are advanced every
    /// bar so a branch that doesn't fire this bar keeps warming up in the
    /// background. Warm-up is the max of the three; the ternary reports
    /// `None` until every source has warmed. See
    /// [`fugazi::indicators::IfElse`].
    IfElse {
        cond: Box<SignalSpec>,
        if_true: Box<ExprSpec>,
        if_false: Box<ExprSpec>,
    },
    Lag {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        periods: usize,
    },
    Diff {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        periods: usize,
    },
    Ratio {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        periods: usize,
    },
    Roc {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        periods: usize,
    },
    RollingMax {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        period: usize,
    },
    RollingMin {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        period: usize,
    },
    /// Logarithm of `source` in `base` (defaults to natural log, `e`).
    /// Emits `None` on samples where the source's output is non-positive.
    Log {
        #[serde(default = "default_source")]
        source: Box<ExprSpec>,
        #[serde(default = "default_log_base")]
        base: Real,
    },
    /// Holds the most recent `Some` output of `source`, re-emitting it on
    /// ticks where `source` returns `None`. Wrap the outermost recursive
    /// smoother of a resampled pipeline so per-base-tick consumers see the
    /// finished higher-timeframe value between boundaries — see
    /// [`fugazi::indicators::Latch`].
    Latch { source: Box<ExprSpec> },
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
    /// chain in [`Latch`](ExprSpec::Latch) so per-base-tick reads see the
    /// finished value between boundaries.
    Resample {
        every: usize,
        inner: Box<ExprSpec>,
        #[serde(default = "default_bar_source")]
        source: Box<ExprSpec>,
    },
    /// Passthrough wrapper that reports `unstable_period() = 0`. The output
    /// and warm-up of `source` are unchanged; the strategy-readiness gate
    /// (which counts up to `stable_period()`) no longer waits for this
    /// subtree's IIR settling tail. The explicit opt-out to the "wait for
    /// every source to be past its unstable tail" safe default; see
    /// [`fugazi::indicators::Unstable`].
    Unstable { source: Box<ExprSpec> },

    // --- calendar accessors (read `atom.time`, emit Real; None when time is
    // absent). Each takes an optional `source` for cross-asset use — the
    // bare form (`!year`) is the default single-series shortcut,
    // `!year { source: !pick { ... } }` reads the picked asset's time.
    /// The Gregorian year (e.g. `2024.0`).
    Year {
        #[serde(default)]
        source: Option<Box<ExprSpec>>,
    },
    /// The Gregorian month, `1.0` (Jan) through `12.0` (Dec).
    Month {
        #[serde(default)]
        source: Option<Box<ExprSpec>>,
    },
    /// The day of the month, `1.0` through `31.0`.
    Day {
        #[serde(default)]
        source: Option<Box<ExprSpec>>,
    },
    /// The hour of the day (UTC), `0.0` through `23.0`.
    Hour {
        #[serde(default)]
        source: Option<Box<ExprSpec>>,
    },
    /// The minute of the hour, `0.0` through `59.0`.
    Minute {
        #[serde(default)]
        source: Option<Box<ExprSpec>>,
    },
    /// The second of the minute, `0.0` through `59.0`.
    Second {
        #[serde(default)]
        source: Option<Box<ExprSpec>>,
    },
    /// ISO 8601 weekday, `1.0` (Monday) through `7.0` (Sunday).
    DayOfWeek {
        #[serde(default)]
        source: Option<Box<ExprSpec>>,
    },
    /// Day of the year, `1.0` through `366.0`.
    DayOfYear {
        #[serde(default)]
        source: Option<Box<ExprSpec>>,
    },
    /// ISO 8601 week of the year, `1.0` through `53.0`.
    WeekOfYear {
        #[serde(default)]
        source: Option<Box<ExprSpec>>,
    },
    /// Calendar quarter, `1.0` through `4.0`.
    Quarter {
        #[serde(default)]
        source: Option<Box<ExprSpec>>,
    },
    /// Unix seconds since the epoch (as a Real).
    UnixSeconds {
        #[serde(default)]
        source: Option<Box<ExprSpec>>,
    },
    /// Unix milliseconds since the epoch (as a Real).
    UnixMillis {
        #[serde(default)]
        source: Option<Box<ExprSpec>>,
    },
    /// The raw bar-open [`Timestamp`] payload (yields
    /// `DynType::Time`, not a scalar). The `Timestamp` twin of
    /// [`ExprSpec::Current`].
    Time {
        #[serde(default)]
        source: Option<Box<ExprSpec>>,
    },
}

impl From<ExprSpecRaw> for ExprSpec {
    fn from(v: ExprSpecRaw) -> Self {
        match v {
            ExprSpecRaw::Close { source } => ExprSpec::Close { source },
            ExprSpecRaw::High { source } => ExprSpec::High { source },
            ExprSpecRaw::Low { source } => ExprSpec::Low { source },
            ExprSpecRaw::Open { source } => ExprSpec::Open { source },
            ExprSpecRaw::Volume { source } => ExprSpec::Volume { source },
            ExprSpecRaw::Typical { source } => ExprSpec::Typical { source },
            ExprSpecRaw::Median { source } => ExprSpec::Median { source },
            ExprSpecRaw::Current { source } => ExprSpec::Current { source },
            ExprSpecRaw::Pick { symbol, freq } => ExprSpec::Pick { symbol, freq },
            ExprSpecRaw::Value(x) => ExprSpec::Value(x),
            ExprSpecRaw::Entry => ExprSpec::Entry,
            ExprSpecRaw::Peak => ExprSpec::Peak,
            ExprSpecRaw::Trough => ExprSpec::Trough,
            ExprSpecRaw::Get { key, source } => ExprSpec::Get { key, source },
            ExprSpecRaw::Ema { source, period } => ExprSpec::Ema { source, period },
            ExprSpecRaw::Sma { source, period } => ExprSpec::Sma { source, period },
            ExprSpecRaw::Rma { source, period } => ExprSpec::Rma { source, period },
            ExprSpecRaw::Wma { source, period } => ExprSpec::Wma { source, period },
            ExprSpecRaw::Hma { source, period } => ExprSpec::Hma { source, period },
            ExprSpecRaw::Rsi { source, period } => ExprSpec::Rsi { source, period },
            ExprSpecRaw::StdDev { source, period } => ExprSpec::StdDev { source, period },
            ExprSpecRaw::Skewness { source, period } => ExprSpec::Skewness { source, period },
            ExprSpecRaw::Kurtosis { source, period } => ExprSpec::Kurtosis { source, period },
            ExprSpecRaw::ZScore { source, period } => ExprSpec::ZScore { source, period },
            ExprSpecRaw::Correlation { lhs, rhs, period } => ExprSpec::Correlation { lhs, rhs, period },
            ExprSpecRaw::Cci { source, period } => ExprSpec::Cci { source, period },
            ExprSpecRaw::Stochastic { source, period } => ExprSpec::Stochastic { source, period },
            ExprSpecRaw::StochRsi { source, rsi_period, stoch_period } => ExprSpec::StochRsi { source, rsi_period, stoch_period },
            ExprSpecRaw::MacdLine { source, fast, slow, signal } => ExprSpec::MacdLine { source, fast, slow, signal },
            ExprSpecRaw::MacdSignal { source, fast, slow, signal } => ExprSpec::MacdSignal { source, fast, slow, signal },
            ExprSpecRaw::MacdHistogram { source, fast, slow, signal } => ExprSpec::MacdHistogram { source, fast, slow, signal },
            ExprSpecRaw::BbUpper { source, period, k } => ExprSpec::BbUpper { source, period, k },
            ExprSpecRaw::BbMiddle { source, period, k } => ExprSpec::BbMiddle { source, period, k },
            ExprSpecRaw::BbLower { source, period, k } => ExprSpec::BbLower { source, period, k },
            ExprSpecRaw::KeltnerUpper { source, candle_source, ema_period, atr_period, multiplier } => ExprSpec::KeltnerUpper { source, candle_source, ema_period, atr_period, multiplier },
            ExprSpecRaw::KeltnerMiddle { source, candle_source, ema_period, atr_period, multiplier } => ExprSpec::KeltnerMiddle { source, candle_source, ema_period, atr_period, multiplier },
            ExprSpecRaw::KeltnerLower { source, candle_source, ema_period, atr_period, multiplier } => ExprSpec::KeltnerLower { source, candle_source, ema_period, atr_period, multiplier },
            ExprSpecRaw::DonchianUpper { high, low, period } => ExprSpec::DonchianUpper { high, low, period },
            ExprSpecRaw::DonchianMiddle { high, low, period } => ExprSpec::DonchianMiddle { high, low, period },
            ExprSpecRaw::DonchianLower { high, low, period } => ExprSpec::DonchianLower { high, low, period },
            ExprSpecRaw::Adx { source, period } => ExprSpec::Adx { source, period },
            ExprSpecRaw::PlusDi { source, period } => ExprSpec::PlusDi { source, period },
            ExprSpecRaw::MinusDi { source, period } => ExprSpec::MinusDi { source, period },
            ExprSpecRaw::DmiPlusDi { source, period } => ExprSpec::DmiPlusDi { source, period },
            ExprSpecRaw::DmiMinusDi { source, period } => ExprSpec::DmiMinusDi { source, period },
            ExprSpecRaw::AroonUp { source, period } => ExprSpec::AroonUp { source, period },
            ExprSpecRaw::AroonDown { source, period } => ExprSpec::AroonDown { source, period },
            ExprSpecRaw::AroonOscillator { source, period } => ExprSpec::AroonOscillator { source, period },
            ExprSpecRaw::Atr { source, period } => ExprSpec::Atr { source, period },
            ExprSpecRaw::Parkinson { source, period } => ExprSpec::Parkinson { source, period },
            ExprSpecRaw::GarmanKlass { source, period } => ExprSpec::GarmanKlass { source, period },
            ExprSpecRaw::RogersSatchell { source, period } => {
                ExprSpec::RogersSatchell { source, period }
            }
            ExprSpecRaw::Mfi { source, period } => ExprSpec::Mfi { source, period },
            ExprSpecRaw::WilliamsR { source, period } => ExprSpec::WilliamsR { source, period },
            ExprSpecRaw::Obv { source } => ExprSpec::Obv { source },
            ExprSpecRaw::Vwap { source } => ExprSpec::Vwap { source },
            ExprSpecRaw::Ad { source } => ExprSpec::Ad { source },
            ExprSpecRaw::TrueRange { source } => ExprSpec::TrueRange { source },
            ExprSpecRaw::Sar { source, step, max } => ExprSpec::Sar { source, step, max },
            ExprSpecRaw::EqualWeight(n) => ExprSpec::EqualWeight(n),
            ExprSpecRaw::VolTarget { target, window, bars_per_year } => ExprSpec::VolTarget { target, window, bars_per_year },
            ExprSpecRaw::AtrRisk { risk_frac, period, atr_multiple } => ExprSpec::AtrRisk { risk_frac, period, atr_multiple },
            ExprSpecRaw::DrawdownThrottle { max_drawdown } => ExprSpec::DrawdownThrottle { max_drawdown },
            ExprSpecRaw::EquityVolTarget { target, window, bars_per_year } => ExprSpec::EquityVolTarget { target, window, bars_per_year },
            ExprSpecRaw::FractionalKelly { kelly_fraction, window } => ExprSpec::FractionalKelly { kelly_fraction, window },
            ExprSpecRaw::Add { lhs, rhs } => ExprSpec::Add { lhs, rhs },
            ExprSpecRaw::Sub { lhs, rhs } => ExprSpec::Sub { lhs, rhs },
            ExprSpecRaw::Mul { lhs, rhs } => ExprSpec::Mul { lhs, rhs },
            ExprSpecRaw::Div { lhs, rhs } => ExprSpec::Div { lhs, rhs },
            ExprSpecRaw::IfElse {
                cond,
                if_true,
                if_false,
            } => ExprSpec::IfElse {
                cond,
                if_true,
                if_false,
            },
            ExprSpecRaw::Lag { source, periods } => ExprSpec::Lag { source, periods },
            ExprSpecRaw::Diff { source, periods } => ExprSpec::Diff { source, periods },
            ExprSpecRaw::Ratio { source, periods } => ExprSpec::Ratio { source, periods },
            ExprSpecRaw::Roc { source, periods } => ExprSpec::Roc { source, periods },
            ExprSpecRaw::RollingMax { source, period } => ExprSpec::RollingMax { source, period },
            ExprSpecRaw::RollingMin { source, period } => ExprSpec::RollingMin { source, period },
            ExprSpecRaw::Log { source, base } => ExprSpec::Log { source, base },
            ExprSpecRaw::Latch { source } => ExprSpec::Latch { source },
            ExprSpecRaw::Resample { every, inner, source } => ExprSpec::Resample { every, inner, source },
            ExprSpecRaw::Unstable { source } => ExprSpec::Unstable { source },
            ExprSpecRaw::Year { source } => ExprSpec::Year { source },
            ExprSpecRaw::Month { source } => ExprSpec::Month { source },
            ExprSpecRaw::Day { source } => ExprSpec::Day { source },
            ExprSpecRaw::Hour { source } => ExprSpec::Hour { source },
            ExprSpecRaw::Minute { source } => ExprSpec::Minute { source },
            ExprSpecRaw::Second { source } => ExprSpec::Second { source },
            ExprSpecRaw::DayOfWeek { source } => ExprSpec::DayOfWeek { source },
            ExprSpecRaw::DayOfYear { source } => ExprSpec::DayOfYear { source },
            ExprSpecRaw::WeekOfYear { source } => ExprSpec::WeekOfYear { source },
            ExprSpecRaw::Quarter { source } => ExprSpec::Quarter { source },
            ExprSpecRaw::UnixSeconds { source } => ExprSpec::UnixSeconds { source },
            ExprSpecRaw::UnixMillis { source } => ExprSpec::UnixMillis { source },
            ExprSpecRaw::Time { source } => ExprSpec::Time { source },
        }
    }
}

impl TryFrom<serde_norway::Value> for ExprSpec {
    type Error = String;

    /// Normalise the incoming YAML value into a [`serde_norway::Value::Tagged`],
    /// then deserialize into [`ExprSpecRaw`].
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
    /// Recursion into `Box<ExprSpec>` fields re-enters this same
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
        let raw: ExprSpecRaw =
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
    source: Option<&ExprSpec>,
    anchor: &Position,
    book: &Book,
    schema: &Arc<Schema>,
) -> AsAtom {
    match source {
        None => AsAtom::new(dyn_indicator::wrap(pick_root())),
        Some(s) => AsAtom::new(s.build(anchor, book, schema)),
    }
}

impl ExprSpec {
    /// Construct the live, runtime-typed source this spec describes as a
    /// `Box<dyn DynIndicator>`. `anchor` is the owning strategy's
    /// [`Position`], shared by any `entry` / `peak` / `trough` leaves in the
    /// tree; `book` is the owning strategy's [`Book`], shared by any
    /// book-anchored sizing recipe (`!drawdown_throttle`, `!equity_vol_target`,
    /// `!fractional_kelly`); `schema` is the overlay [`Schema`] the atom
    /// stream carries, used by `!get { key }` to look up the column's
    /// declared [`OverlayType`] and dispatch to the right typed leaf.
    pub fn build(
        &self,
        anchor: &Position,
        book: &Book,
        schema: &Arc<Schema>,
    ) -> Box<dyn DynIndicator> {
        use ExprSpec::*;
        // Recursive-build shorthands: build `s`, view it as a library-typed
        // `Indicator<Input=Snapshot, Output=Real>` (or Candle) so it drops
        // into a concrete library constructor.
        let real = |s: &ExprSpec| AsReal::new(s.build(anchor, book, schema));
        let candle = |s: &ExprSpec| AsCandle::new(s.build(anchor, book, schema));
        // The `Pick`-shaped `source:` field on every atom-input leaf.
        let atom_src = |source: Option<&Box<ExprSpec>>| {
            atom_source_of(source.map(|b| &**b), anchor, book, schema)
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

            Value(ValueLit::Real(x)) => dyn_indicator::wrap(self::Value::<Snapshot<String>>::new(*x)),
            Value(ValueLit::Str(s)) => {
                dyn_indicator::wrap(ValueStr::<Snapshot<String>>::new(s.as_str()))
            }
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
            Skewness { source, period } => {
                dyn_indicator::wrap(self::Skewness::new(real(source), *period))
            }
            Kurtosis { source, period } => {
                dyn_indicator::wrap(self::Kurtosis::new(real(source), *period))
            }
            ZScore { source, period } => {
                dyn_indicator::wrap(self::ZScore::new(real(source), *period))
            }
            Correlation { lhs, rhs, period } => {
                dyn_indicator::wrap(self::Correlation::new(real(lhs), real(rhs), *period))
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
            Parkinson { source, period } => {
                dyn_indicator::wrap(self::Parkinson::new(candle(source), *period))
            }
            GarmanKlass { source, period } => {
                dyn_indicator::wrap(self::GarmanKlass::new(candle(source), *period))
            }
            RogersSatchell { source, period } => {
                dyn_indicator::wrap(self::RogersSatchell::new(candle(source), *period))
            }
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

            EqualWeight(n_legs) => dyn_indicator::wrap(
                fugazi::indicators::sizing::equal_weight::<String>(*n_legs),
            ),
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
            DrawdownThrottle { max_drawdown } => {
                dyn_indicator::wrap(fugazi::indicators::sizing::drawdown_throttle::<String>(
                    book,
                    *max_drawdown,
                ))
            }
            EquityVolTarget {
                target,
                window,
                bars_per_year,
            } => dyn_indicator::wrap(
                fugazi::indicators::sizing::equity_vol_target::<String>(
                    book,
                    *target,
                    *window,
                    *bars_per_year,
                ),
            ),
            FractionalKelly {
                kelly_fraction,
                window,
            } => dyn_indicator::wrap(fugazi::indicators::sizing::fractional_kelly::<String>(
                book,
                *kelly_fraction,
                *window,
            )),

            Add { lhs, rhs } => dyn_indicator::wrap(real(lhs).add(real(rhs))),
            Sub { lhs, rhs } => dyn_indicator::wrap(real(lhs).sub(real(rhs))),
            Mul { lhs, rhs } => dyn_indicator::wrap(real(lhs).mul(real(rhs))),
            Div { lhs, rhs } => dyn_indicator::wrap(real(lhs).div(real(rhs))),
            IfElse {
                cond,
                if_true,
                if_false,
            } => {
                let cond_ind = AsBool::new(cond.build(anchor, book, schema));
                let t_ind = real(if_true);
                let f_ind = real(if_false);
                dyn_indicator::wrap(self::IfElse::new(cond_ind, t_ind, f_ind))
            }
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
                let inner = AsReal::new(source.build(anchor, book, schema));
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
                let inner_dyn = inner.build(anchor, book, schema);
                dyn_indicator::chain(resample_dyn, inner_dyn)
            }
            Unstable { source } => dyn_indicator::unstable_wrap(source.build(anchor, book, schema)),

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
