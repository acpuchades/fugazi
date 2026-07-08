//! Calendar field accessors: source indicators that decompose an
//! [`Atom`]'s bar-open [`Timestamp`] into a scalar calendar field.
//!
//! All share the [`CalendarField`] marker pattern (parallel to
//! [`Field`](super::Field) for candle scalars), so a new accessor is a trait
//! impl rather than a new type.
//!
//! Every accessor is generic over an atom-emitting source `S` (default
//! [`Identity<Atom>`], i.e. the raw bar stream). When `atom.time` is `None`
//! (e.g. a synthetic candle fed without wall-clock metadata), `update` returns
//! `None` — same shape as a not-yet-warm indicator result, so downstream
//! comparisons/signals stay `None` until times are provided.
//!
//! Two boolean signals ([`IsWeekday`], [`IsWeekend`]) sit alongside the numeric
//! decompositions; the rest can be expressed with the existing comparison
//! surface (`!eq { lhs: !day_of_week, rhs: !value 1 }` = "Monday").
//!
//! # A note on timeframe: daily and higher
//!
//! `atom.time` is deliberately [`Option<Timestamp>`]. Not every bar-stream
//! driver populates it — a source that only carries date strings (CSVs
//! without an ISO datetime column, some daily-EOD feeds) may hand fugazi
//! bare [`Atom::new`] values with no time attached. In that mode **every
//! calendar accessor here returns `None` for every bar**, and any signal
//! composed on top (`day_of_week().eq(1)`, `is_weekday()`) reads as
//! `false` (the `None`-until-warm convention).
//!
//! Even when the driver stamps a time, daily-and-higher bars conventionally
//! sit at 00:00 UTC of the session open, so [`Hour`], [`Minute`] and
//! [`Second`] read as identically `0.0` and carry no information — only
//! [`Year`] through [`DayOfWeek`] / [`Quarter`] etc. are meaningful.
//! Callers pushing sub-daily calendar signals into a daily strategy should
//! therefore prefer the day-and-above accessors, and treat sub-day reads as
//! nominal rather than session-relative.

use std::marker::PhantomData;

use time::OffsetDateTime;

use crate::indicator::Indicator;
use crate::indicators::Identity;
use crate::types::{Atom, Real, Timestamp};

/// Selects a scalar calendar field from a [`Timestamp`], projected via
/// `time::OffsetDateTime` at UTC.
///
/// Twin of [`CandleField`](super::CandleField). A new decomposition is a
/// trait impl over a zero-sized marker.
pub trait CalendarField {
    fn get(dt: OffsetDateTime) -> Real;
}

/// A source indicator that extracts one [`CalendarField`] from each bar's
/// `atom.time`.
///
/// Emits `None` on bars whose `time` is `None`. Use the aliases
/// ([`Year`], [`Month`], [`Day`], [`Hour`], [`Minute`], [`Second`],
/// [`DayOfWeek`], [`DayOfYear`], [`DayOfMonth`], [`WeekOfYear`], [`Quarter`],
/// [`UnixSeconds`], [`UnixMillis`]).
///
/// Generic over the atom-emitting source `S` (default [`Identity<Atom>`]).
#[derive(Debug, Clone)]
pub struct Calendar<F, S = Identity<Atom>> {
    source: S,
    /// Latest extracted value; `None` before the first bar or if the last
    /// bar's `time` was absent.
    pub value: Option<Real>,
    _field: PhantomData<fn() -> F>,
}

impl<F> Calendar<F, Identity<Atom>> {
    pub fn new() -> Self {
        Self::of(Identity::new())
    }
}

impl<F, S> Calendar<F, S> {
    /// Constructs a [`Calendar`] rooted on a custom atom-emitting source.
    pub fn of(source: S) -> Self {
        Self {
            source,
            value: None,
            _field: PhantomData,
        }
    }
}

impl<F> Default for Calendar<F, Identity<Atom>> {
    fn default() -> Self {
        Self::new()
    }
}

impl<F: CalendarField, S: Indicator<Output = Atom>> Indicator for Calendar<F, S> {
    type Input = S::Input;
    type Output = Real;

    fn update(&mut self, input: S::Input) -> Option<Real> {
        self.value = self
            .source
            .update(input)
            .and_then(|a| a.time.map(|t| F::get(t.to_datetime())));
        self.value
    }

    fn value(&self) -> Option<Real> {
        self.value
    }

    fn warm_up_period(&self) -> usize {
        self.source.warm_up_period().max(1)
    }

    fn unstable_period(&self) -> usize {
        self.source.unstable_period()
    }

    fn reset(&mut self) {
        self.source.reset();
        self.value = None;
    }
}

// ---------------------------------------------------------------------------
// Field markers + type aliases
// ---------------------------------------------------------------------------

/// The Gregorian year (e.g. `2024.0`).
#[derive(Debug, Clone, Copy)]
pub struct YearField;
impl CalendarField for YearField {
    fn get(dt: OffsetDateTime) -> Real {
        dt.year() as Real
    }
}

/// The Gregorian month, `1.0` (January) through `12.0` (December).
#[derive(Debug, Clone, Copy)]
pub struct MonthField;
impl CalendarField for MonthField {
    fn get(dt: OffsetDateTime) -> Real {
        u8::from(dt.month()) as Real
    }
}

/// The day of the month, `1.0` through `31.0`.
#[derive(Debug, Clone, Copy)]
pub struct DayOfMonthField;
impl CalendarField for DayOfMonthField {
    fn get(dt: OffsetDateTime) -> Real {
        dt.day() as Real
    }
}

/// The hour of the day (UTC), `0.0` through `23.0`.
#[derive(Debug, Clone, Copy)]
pub struct HourField;
impl CalendarField for HourField {
    fn get(dt: OffsetDateTime) -> Real {
        dt.hour() as Real
    }
}

/// The minute of the hour, `0.0` through `59.0`.
#[derive(Debug, Clone, Copy)]
pub struct MinuteField;
impl CalendarField for MinuteField {
    fn get(dt: OffsetDateTime) -> Real {
        dt.minute() as Real
    }
}

/// The second of the minute, `0.0` through `59.0` (never `60`: no leap-second
/// handling on the `time` crate side either).
#[derive(Debug, Clone, Copy)]
pub struct SecondField;
impl CalendarField for SecondField {
    fn get(dt: OffsetDateTime) -> Real {
        dt.second() as Real
    }
}

/// ISO 8601 weekday, `1.0` (Monday) through `7.0` (Sunday).
#[derive(Debug, Clone, Copy)]
pub struct DayOfWeekField;
impl CalendarField for DayOfWeekField {
    fn get(dt: OffsetDateTime) -> Real {
        dt.weekday().number_from_monday() as Real
    }
}

/// Day of the year, `1.0` (January 1) through `366.0` (December 31 of a
/// leap year).
#[derive(Debug, Clone, Copy)]
pub struct DayOfYearField;
impl CalendarField for DayOfYearField {
    fn get(dt: OffsetDateTime) -> Real {
        dt.ordinal() as Real
    }
}

/// ISO 8601 week of the year, `1.0` through `53.0`. Weeks start on Monday and
/// week 1 is the one containing the first Thursday of the year.
#[derive(Debug, Clone, Copy)]
pub struct WeekOfYearField;
impl CalendarField for WeekOfYearField {
    fn get(dt: OffsetDateTime) -> Real {
        dt.iso_week() as Real
    }
}

/// Calendar quarter, `1.0` (Jan–Mar) through `4.0` (Oct–Dec).
#[derive(Debug, Clone, Copy)]
pub struct QuarterField;
impl CalendarField for QuarterField {
    fn get(dt: OffsetDateTime) -> Real {
        let m = u8::from(dt.month()) as Real;
        ((m - 1.0) / 3.0).floor() + 1.0
    }
}

/// Unix seconds since the epoch, as a real (may lose sub-millisecond fraction).
#[derive(Debug, Clone, Copy)]
pub struct UnixSecondsField;
impl CalendarField for UnixSecondsField {
    fn get(dt: OffsetDateTime) -> Real {
        dt.unix_timestamp() as Real
    }
}

/// Unix milliseconds since the epoch, as a real.
///
/// Losslessly representable up to `2^53` ms (~year 285 428) — well beyond any
/// realistic bar timestamp.
#[derive(Debug, Clone, Copy)]
pub struct UnixMillisField;
impl CalendarField for UnixMillisField {
    fn get(dt: OffsetDateTime) -> Real {
        let nanos = dt.unix_timestamp_nanos();
        (nanos / 1_000_000) as Real
    }
}

/// The Gregorian year (e.g. `2024.0`).
pub type Year<S = Identity<Atom>> = Calendar<YearField, S>;
/// The Gregorian month, `1.0` (January) through `12.0` (December).
pub type Month<S = Identity<Atom>> = Calendar<MonthField, S>;
/// The day of the month, `1.0` through `31.0`.
pub type DayOfMonth<S = Identity<Atom>> = Calendar<DayOfMonthField, S>;
/// Alias for [`DayOfMonth`].
pub type Day<S = Identity<Atom>> = DayOfMonth<S>;
/// The hour of the day (UTC), `0.0` through `23.0`.
pub type Hour<S = Identity<Atom>> = Calendar<HourField, S>;
/// The minute of the hour, `0.0` through `59.0`.
pub type Minute<S = Identity<Atom>> = Calendar<MinuteField, S>;
/// The second of the minute, `0.0` through `59.0`.
pub type Second<S = Identity<Atom>> = Calendar<SecondField, S>;
/// ISO 8601 weekday, `1.0` (Monday) through `7.0` (Sunday).
pub type DayOfWeek<S = Identity<Atom>> = Calendar<DayOfWeekField, S>;
/// Day of the year, `1.0` through `366.0`.
pub type DayOfYear<S = Identity<Atom>> = Calendar<DayOfYearField, S>;
/// ISO 8601 week of the year, `1.0` through `53.0`.
pub type WeekOfYear<S = Identity<Atom>> = Calendar<WeekOfYearField, S>;
/// Calendar quarter, `1.0` through `4.0`.
pub type Quarter<S = Identity<Atom>> = Calendar<QuarterField, S>;
/// Unix seconds since the epoch.
pub type UnixSeconds<S = Identity<Atom>> = Calendar<UnixSecondsField, S>;
/// Unix milliseconds since the epoch.
pub type UnixMillis<S = Identity<Atom>> = Calendar<UnixMillisField, S>;

// ---------------------------------------------------------------------------
// Timestamp leaf (yields the raw Timestamp payload)
// ---------------------------------------------------------------------------

/// A pass-through source over the [`Atom::time`] field, yielding the raw
/// [`Timestamp`] payload (not a scalar).
///
/// The [`Timestamp`] twin of [`CurrentBar`](super::CurrentBar) — a leaf that
/// carries the bar-open time forward into a chain that expects a `Timestamp`.
/// Emits `None` on bars whose `time` is `None`; otherwise emits the same
/// `Timestamp` every read.
///
/// Generic over the atom-emitting source `S` (default [`Identity<Atom>`]).
#[derive(Debug, Clone)]
pub struct CurrentTime<S = Identity<Atom>> {
    source: S,
    /// Latest [`Timestamp`] seen; `None` before the first bar or if the last
    /// bar's `time` was absent.
    pub value: Option<Timestamp>,
}

impl CurrentTime<Identity<Atom>> {
    pub fn new() -> Self {
        Self::of(Identity::new())
    }
}

impl<S> CurrentTime<S> {
    /// Constructs a [`CurrentTime`] rooted on a custom atom-emitting source.
    pub fn of(source: S) -> Self {
        Self {
            source,
            value: None,
        }
    }
}

impl Default for CurrentTime<Identity<Atom>> {
    fn default() -> Self {
        Self::new()
    }
}

impl<S: Indicator<Output = Atom>> Indicator for CurrentTime<S> {
    type Input = S::Input;
    type Output = Timestamp;

    fn update(&mut self, input: S::Input) -> Option<Timestamp> {
        self.value = self.source.update(input).and_then(|a| a.time);
        self.value
    }

    fn value(&self) -> Option<Timestamp> {
        self.value
    }

    fn warm_up_period(&self) -> usize {
        self.source.warm_up_period().max(1)
    }

    fn unstable_period(&self) -> usize {
        self.source.unstable_period()
    }

    fn reset(&mut self) {
        self.source.reset();
        self.value = None;
    }
}

// ---------------------------------------------------------------------------
// Boolean calendar signals
// ---------------------------------------------------------------------------

/// True on Monday through Friday, false on Saturday or Sunday. `None` on
/// bars whose `time` is `None` (matching the `None`-until-warm convention).
///
/// Generic over the atom-emitting source `S` (default [`Identity<Atom>`]).
#[derive(Debug, Clone)]
pub struct IsWeekday<S = Identity<Atom>> {
    source: S,
    pub value: Option<bool>,
}

impl IsWeekday<Identity<Atom>> {
    pub fn new() -> Self {
        Self::of(Identity::new())
    }
}

impl<S> IsWeekday<S> {
    /// Constructs an [`IsWeekday`] rooted on a custom atom-emitting source.
    pub fn of(source: S) -> Self {
        Self {
            source,
            value: None,
        }
    }
}

impl Default for IsWeekday<Identity<Atom>> {
    fn default() -> Self {
        Self::new()
    }
}

impl<S: Indicator<Output = Atom>> Indicator for IsWeekday<S> {
    type Input = S::Input;
    type Output = bool;

    fn update(&mut self, input: S::Input) -> Option<bool> {
        self.value = self.source.update(input).and_then(|a| {
            a.time.map(|t| {
                let d = t.to_datetime().weekday().number_from_monday();
                d <= 5
            })
        });
        self.value
    }

    fn value(&self) -> Option<bool> {
        self.value
    }

    fn warm_up_period(&self) -> usize {
        self.source.warm_up_period().max(1)
    }

    fn unstable_period(&self) -> usize {
        self.source.unstable_period()
    }

    fn reset(&mut self) {
        self.source.reset();
        self.value = None;
    }
}

/// True on Saturday or Sunday, false Monday through Friday. `None` on bars
/// whose `time` is `None`.
///
/// Generic over the atom-emitting source `S` (default [`Identity<Atom>`]).
#[derive(Debug, Clone)]
pub struct IsWeekend<S = Identity<Atom>> {
    source: S,
    pub value: Option<bool>,
}

impl IsWeekend<Identity<Atom>> {
    pub fn new() -> Self {
        Self::of(Identity::new())
    }
}

impl<S> IsWeekend<S> {
    /// Constructs an [`IsWeekend`] rooted on a custom atom-emitting source.
    pub fn of(source: S) -> Self {
        Self {
            source,
            value: None,
        }
    }
}

impl Default for IsWeekend<Identity<Atom>> {
    fn default() -> Self {
        Self::new()
    }
}

impl<S: Indicator<Output = Atom>> Indicator for IsWeekend<S> {
    type Input = S::Input;
    type Output = bool;

    fn update(&mut self, input: S::Input) -> Option<bool> {
        self.value = self.source.update(input).and_then(|a| {
            a.time.map(|t| {
                let d = t.to_datetime().weekday().number_from_monday();
                d >= 6
            })
        });
        self.value
    }

    fn value(&self) -> Option<bool> {
        self.value
    }

    fn warm_up_period(&self) -> usize {
        self.source.warm_up_period().max(1)
    }

    fn unstable_period(&self) -> usize {
        self.source.unstable_period()
    }

    fn reset(&mut self) {
        self.source.reset();
        self.value = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Candle;

    fn bar_at(ms: i64) -> Atom {
        Atom::with_time(
            Candle::new(1.0, 1.0, 1.0, 1.0, 0.0),
            Timestamp(ms),
        )
    }

    fn bare_bar() -> Atom {
        Atom::new(Candle::new(1.0, 1.0, 1.0, 1.0, 0.0))
    }

    /// 2024-03-15 12:34:56 UTC = 1_710_506_096_000 ms — a Friday, Q1, DOY 75.
    const REF_MS: i64 = 1_710_506_096_000;

    #[test]
    fn year_month_day_extract() {
        let atom = bar_at(REF_MS);
        assert_eq!(Year::new().update(atom.clone()), Some(2024.0));
        assert_eq!(Month::new().update(atom.clone()), Some(3.0));
        assert_eq!(DayOfMonth::new().update(atom), Some(15.0));
    }

    #[test]
    fn hour_minute_second_extract() {
        let atom = bar_at(REF_MS);
        assert_eq!(Hour::new().update(atom.clone()), Some(12.0));
        assert_eq!(Minute::new().update(atom.clone()), Some(34.0));
        assert_eq!(Second::new().update(atom), Some(56.0));
    }

    #[test]
    fn day_of_week_is_iso_monday_one() {
        let atom = bar_at(REF_MS); // Friday
        assert_eq!(DayOfWeek::new().update(atom), Some(5.0));
    }

    #[test]
    fn day_of_year_ordinal_and_quarter() {
        let atom = bar_at(REF_MS);
        assert_eq!(DayOfYear::new().update(atom.clone()), Some(75.0));
        assert_eq!(Quarter::new().update(atom), Some(1.0));
    }

    #[test]
    fn quarter_covers_all_months() {
        for (m_ms_offset, expected_q) in [
            (0i64, 1.0),                    // Jan 1
            (32 * 86_400_000, 1.0),         // Feb 2
            (60 * 86_400_000, 1.0),         // Mar 2 (non-leap 2023 window; day 60 = Mar 2 2023)
            (91 * 86_400_000, 2.0),         // Apr 2
            (181 * 86_400_000, 3.0),        // Jul 1
            (274 * 86_400_000, 4.0),        // Oct 1
        ] {
            let base = 1_672_531_200_000; // 2023-01-01 UTC
            let atom = bar_at(base + m_ms_offset);
            assert_eq!(Quarter::new().update(atom), Some(expected_q));
        }
    }

    #[test]
    fn unix_ms_and_seconds_roundtrip() {
        let atom = bar_at(REF_MS);
        assert_eq!(UnixMillis::new().update(atom.clone()), Some(REF_MS as Real));
        assert_eq!(
            UnixSeconds::new().update(atom),
            Some((REF_MS / 1000) as Real)
        );
    }

    #[test]
    fn missing_time_yields_none() {
        let mut y = Year::new();
        assert_eq!(y.update(bare_bar()), None);
        assert_eq!(y.value(), None);
        let mut d = DayOfWeek::new();
        assert_eq!(d.update(bare_bar()), None);
    }

    #[test]
    fn current_time_passthrough() {
        let mut t = CurrentTime::new();
        assert_eq!(t.update(bar_at(REF_MS)), Some(Timestamp(REF_MS)));
        assert_eq!(t.update(bare_bar()), None);
    }

    #[test]
    fn weekday_weekend_signals() {
        // 2024-03-15 is a Friday.
        let fri = bar_at(REF_MS);
        assert_eq!(IsWeekday::new().update(fri.clone()), Some(true));
        assert_eq!(IsWeekend::new().update(fri), Some(false));
        // 2024-03-16 is a Saturday (add one day).
        let sat = bar_at(REF_MS + 86_400_000);
        assert_eq!(IsWeekday::new().update(sat.clone()), Some(false));
        assert_eq!(IsWeekend::new().update(sat), Some(true));
        // Missing time.
        assert_eq!(IsWeekday::new().update(bare_bar()), None);
        assert_eq!(IsWeekend::new().update(bare_bar()), None);
    }

    #[test]
    fn warm_up_and_reset() {
        let mut y = Year::new();
        assert_eq!(y.warm_up_period(), 1);
        y.update(bar_at(REF_MS));
        assert_eq!(y.value(), Some(2024.0));
        y.reset();
        assert_eq!(y.value(), None);
    }
}
