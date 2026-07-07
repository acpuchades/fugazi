//! YAML-deserializable [`SourceSpec`] — the real-valued source layer.
//!
//! Split out of `spec/mod.rs`; kept in `crate::spec::source` so paths like
//! `crate::spec::SourceSpec` still resolve via the `pub use` in `mod.rs`.

use std::sync::Arc;

use serde::Deserialize;

use fugazi::indicators::{
    Ad, Adx, AdxValue, Aroon, AroonValue, Atr, Bollinger, BollingerValue, Cci, Component, Current,
    CurrentTime, Day, DayOfWeek, DayOfYear, Dmi, DmiValue, Donchian, DonchianValue, Ema, GetBool,
    GetReal, GetStr, Hma, Hour, Keltner, KeltnerValue, Latch, Macd, MacdValue, Mfi, Minute, Month,
    Obv, Position, Quarter, Resample, Rma, Rsi, Sar, Second, Sma, StdDev, StochRsi, Stochastic,
    TrueRange, UnixMillis, UnixSeconds, Value, Vwap, WeekOfYear, WilliamsR, Wma, Year,
};
use fugazi::prelude::*;

use crate::dyn_indicator::{self, AsCandle, AsReal, DynIndicator};

pub(super) fn default_source() -> Box<SourceSpec> {
    Box::new(SourceSpec::Close)
}
pub(super) fn default_high() -> Box<SourceSpec> {
    Box::new(SourceSpec::High)
}
pub(super) fn default_low() -> Box<SourceSpec> {
    Box::new(SourceSpec::Low)
}
/// Default candle source for bar indicators — the current bar itself.
pub(super) fn default_bar_source() -> Box<SourceSpec> {
    Box::new(SourceSpec::Current)
}

// ---------------------------------------------------------------------------
// Real-valued sources
// ---------------------------------------------------------------------------

/// A real-valued source over a candle stream — the YAML form of any
/// `Indicator<Input = Candle, Output = Real>`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceSpec {
    // --- candle-field leaves (bare strings) ---
    Close,
    High,
    Low,
    Open,
    Volume,
    Typical,
    Median,
    /// The current bar itself — the whole [`Candle`], not a scalar. The default
    /// bar source of every bar-consuming indicator (`!atr`, `!obv`, `!adx`, …);
    /// wrap in [`SourceSpec::Resample`] to lift a downstream bar indicator onto
    /// a higher timeframe.
    Current,
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
    // absent). See [`fugazi::indicators::calendar`] for the underlying
    // primitives.
    /// The Gregorian year (e.g. `2024.0`).
    Year,
    /// The Gregorian month, `1.0` (Jan) through `12.0` (Dec).
    Month,
    /// The day of the month, `1.0` through `31.0`.
    Day,
    /// The hour of the day (UTC), `0.0` through `23.0`.
    Hour,
    /// The minute of the hour, `0.0` through `59.0`.
    Minute,
    /// The second of the minute, `0.0` through `59.0`.
    Second,
    /// ISO 8601 weekday, `1.0` (Monday) through `7.0` (Sunday).
    DayOfWeek,
    /// Day of the year, `1.0` through `366.0`.
    DayOfYear,
    /// ISO 8601 week of the year, `1.0` through `53.0`.
    WeekOfYear,
    /// Calendar quarter, `1.0` through `4.0`.
    Quarter,
    /// Unix seconds since the epoch (as a Real).
    UnixSeconds,
    /// Unix milliseconds since the epoch (as a Real).
    UnixMillis,
    /// The raw bar-open [`Timestamp`] payload (yields
    /// `DynType::Time`, not a scalar). The `Timestamp` twin of
    /// [`SourceSpec::Current`].
    Time,
}

impl SourceSpec {
    /// Construct the live, runtime-typed source this spec describes as a
    /// `Box<dyn DynIndicator>`. `anchor` is the owning strategy's
    /// [`Position`], shared by any `entry` / `peak` / `trough` leaves in the
    /// tree; `schema` is the overlay [`Schema`] the atom stream carries, used
    /// by `!get { key }` to look up the column's declared [`OverlayType`] and
    /// dispatch to the right typed leaf.
    ///
    /// Most variants produce `output_type() == DynType::Real`; the exceptions
    /// are `Get`-shaped ones (whose output type follows the schema), and the
    /// `Current` candle passthrough which produces `Candle`.
    pub fn build(&self, anchor: &Position, schema: &Arc<Schema>) -> Box<dyn DynIndicator> {
        use SourceSpec::*;
        // Recursive-build shorthand: build `s`, view it as a library-typed
        // `Indicator<Input=Atom, Output=Real>` so it drops into a concrete
        // library constructor.
        let real = |s: &SourceSpec| AsReal::new(s.build(anchor, schema));
        // Same for a candle-output source (the input of every bar indicator).
        let candle = |s: &SourceSpec| AsCandle::new(s.build(anchor, schema));

        match self {
            Close => dyn_indicator::wrap(self::Current::close()),
            High => dyn_indicator::wrap(self::Current::high()),
            Low => dyn_indicator::wrap(self::Current::low()),
            Open => dyn_indicator::wrap(self::Current::open()),
            Volume => dyn_indicator::wrap(self::Current::volume()),
            Typical => dyn_indicator::wrap(self::Current::typical()),
            Median => dyn_indicator::wrap(self::Current::median()),
            Current => dyn_indicator::wrap(self::Current::candle()),
            Value(x) => dyn_indicator::wrap(self::Value::<Atom>::new(*x)),
            Entry => dyn_indicator::wrap(anchor.entry()),
            Peak => dyn_indicator::wrap(anchor.peak()),
            Trough => dyn_indicator::wrap(anchor.trough()),
            Get { key } => build_get(schema, key),

            Ema { source, period } => dyn_indicator::wrap(self::Ema::new(real(source), *period)),
            Sma { source, period } => dyn_indicator::wrap(self::Sma::new(real(source), *period)),
            Rma { source, period } => dyn_indicator::wrap(self::Rma::new(real(source), *period)),
            Wma { source, period } => dyn_indicator::wrap(self::Wma::new(real(source), *period)),
            Hma { source, period } => dyn_indicator::wrap(self::Hma::new(real(source), *period)),
            Rsi { source, period } => dyn_indicator::wrap(self::Rsi::new(real(source), *period)),
            StdDev { source, period } => dyn_indicator::wrap(self::StdDev::new(real(source), *period)),
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
            Sar { source, step, max } => dyn_indicator::wrap(self::Sar::new(candle(source), *step, *max)),

            Add { lhs, rhs } => dyn_indicator::wrap(real(lhs).add(real(rhs))),
            Sub { lhs, rhs } => dyn_indicator::wrap(real(lhs).sub(real(rhs))),
            Mul { lhs, rhs } => dyn_indicator::wrap(real(lhs).mul(real(rhs))),
            Div { lhs, rhs } => dyn_indicator::wrap(real(lhs).div(real(rhs))),
            Lag { source, periods } => dyn_indicator::wrap(real(source).lag(*periods)),
            Diff { source, periods } => dyn_indicator::wrap(real(source).diff(*periods)),
            Ratio { source, periods } => dyn_indicator::wrap(real(source).ratio(*periods)),
            Roc { source, periods } => dyn_indicator::wrap(real(source).roc(*periods)),
            RollingMax { source, period } => dyn_indicator::wrap(real(source).rolling_max(*period)),
            RollingMin { source, period } => dyn_indicator::wrap(real(source).rolling_min(*period)),
            Latch { source } => {
                // Wrap the built source in the library's Latch — preserves
                // Real output; warm-up / unstable pass through unchanged.
                let inner = AsReal::new(source.build(anchor, schema));
                dyn_indicator::wrap(self::Latch::new(inner))
            }
            Resample {
                every,
                inner,
                source,
            } => {
                assert!(*every > 0, "resample every must be greater than zero");
                // `Resample<S>` is `S::Input -> Candle`, and `DynValue` carries
                // both `Atom` and `Candle`. The runtime `chain` glues the
                // Candle output into the inner source's Atom input via the
                // `Candle -> Atom` lift in `TryFrom<DynValue> for Atom` — so a
                // downstream Atom-consuming source (`close`, `!ema`, …) sees
                // the resampled candle as an atom with no overlays.
                let candle_src = candle(source);
                let resample_dyn = dyn_indicator::wrap(self::Resample::new(candle_src, *every));
                let inner_dyn = inner.build(anchor, schema);
                dyn_indicator::chain(resample_dyn, inner_dyn)
            }
            Unstable { source } => dyn_indicator::unstable_wrap(source.build(anchor, schema)),

            Year => dyn_indicator::wrap(self::Year::new()),
            Month => dyn_indicator::wrap(self::Month::new()),
            Day => dyn_indicator::wrap(self::Day::new()),
            Hour => dyn_indicator::wrap(self::Hour::new()),
            Minute => dyn_indicator::wrap(self::Minute::new()),
            Second => dyn_indicator::wrap(self::Second::new()),
            DayOfWeek => dyn_indicator::wrap(self::DayOfWeek::new()),
            DayOfYear => dyn_indicator::wrap(self::DayOfYear::new()),
            WeekOfYear => dyn_indicator::wrap(self::WeekOfYear::new()),
            Quarter => dyn_indicator::wrap(self::Quarter::new()),
            UnixSeconds => dyn_indicator::wrap(self::UnixSeconds::new()),
            UnixMillis => dyn_indicator::wrap(self::UnixMillis::new()),
            Time => dyn_indicator::wrap(self::CurrentTime::new()),
        }
    }
}

/// Build a `!get { key }` leaf: look up the column's declared [`OverlayType`]
/// in `schema` and dispatch to the matching typed [`GetReal`] / [`GetBool`] /
/// [`GetStr`] leaf.
///
/// Panics with a helpful message if `key` isn't registered — the message
/// lists the schema's registered keys so a typo is easy to spot. The message
/// distinguishes the empty-schema case ("no overlay side channel — feed
/// `--series` or `file:` data with additional columns to attach overlays")
/// from the non-empty case ("registered: a, b, c").
fn build_get(schema: &Arc<Schema>, key: &str) -> Box<dyn DynIndicator> {
    match schema.type_of_key(key) {
        Some(OverlayType::Real) => dyn_indicator::wrap(GetReal::new(schema, key)),
        Some(OverlayType::Bool) => dyn_indicator::wrap(GetBool::new(schema, key)),
        Some(OverlayType::Str) => dyn_indicator::wrap(GetStr::new(schema, key)),
        None => {
            let registered: Vec<&str> = schema.keys().collect();
            if registered.is_empty() {
                panic!(
                    "!get {{ key: {key:?} }}: no overlay side channel is bound \
                     — feed `--series` data or a `file:` source that carries \
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
