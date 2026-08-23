//! Time vocabulary: the UTC millisecond [`Timestamp`] every dated bar carries,
//! and the [`Frequency`] enum that names its bar cadence.

use std::fmt;
use std::str::FromStr;

/// A UTC millisecond timestamp (Unix epoch).
///
/// Kept as a flat `i64` on purpose: it matches Binance's native representation,
/// stays `Copy`, and keeps `time::OffsetDateTime` out of the pure core's ABI —
/// callers that want a datetime go through [`Timestamp::to_datetime`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(pub i64);

impl Timestamp {
    /// The current UTC time, in milliseconds since the Unix epoch.
    pub fn now() -> Self {
        Self::from_datetime(::time::OffsetDateTime::now_utc())
    }

    /// Convert a `time::OffsetDateTime` to a millisecond epoch stamp.
    pub fn from_datetime(dt: ::time::OffsetDateTime) -> Self {
        let nanos = dt.unix_timestamp_nanos();
        Self((nanos / 1_000_000) as i64)
    }

    /// Reconstruct a `time::OffsetDateTime` at UTC from this millisecond stamp,
    /// or `None` when the stamp lands outside the calendar `time` can express.
    ///
    /// **An `i64` of milliseconds does not fit in an `OffsetDateTime`**, which
    /// is where this used to `expect`. `time` tops out at year 9999 —
    /// `253_402_300_800_000` ms — and an `i64` reaches 292 million years, so
    /// three quarters of the type's range aborts. That is not a theoretical
    /// gap: a `time` column in **nanoseconds** is what `pandas`' and `polars`'
    /// `datetime64[ns]` produce when cast to an integer, `parse_time_to_millis`
    /// passes any stamp past `1e11` through as milliseconds by design (it cannot
    /// tell nanoseconds from a far-future date), and the first `!is_weekday` in
    /// the document then killed the run with a raw panic message.
    ///
    /// So this is `Option`, and every caller degrades: a calendar accessor reads
    /// `None` — the same answer it already gives an undated bar — and a
    /// formatter falls back to the raw number.
    pub fn to_datetime(self) -> Option<::time::OffsetDateTime> {
        let nanos = (self.0 as i128) * 1_000_000;
        ::time::OffsetDateTime::from_unix_timestamp_nanos(nanos).ok()
    }
}

/// A bar cadence as an integer multiplier and unit — `5m`, `4h`, `1d`, `1w`,
/// `1M`. `M` for month is uppercase to keep `m` unambiguously "minute".
///
/// Ordered by *duration* rather than by variant tag, so
/// `Frequency::Minute(120) > Frequency::Hour(1)` behaves the way a reader
/// would expect. `Hash + Eq` — usable as a HashMap key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Frequency {
    Minute(u32),
    Hour(u32),
    Day(u32),
    Week(u32),
    Month(u32),
}

impl Frequency {
    /// The approximate seconds a bar of this cadence spans, using **calendar**
    /// conventions (30-day month, 7-day week). Used as the primary total-
    /// order key by [`Ord`] so cadences sort by duration regardless of
    /// variant, which keeps `Frequency::Minute(120) > Frequency::Hour(1)` (a
    /// derived `Ord` would order them lexicographically by variant tag and
    /// get it wrong).
    ///
    /// **Calendar, not trading.** Distinct from
    /// [`AssetClass::trading_seconds_per_bar`](crate::spec::calendar::AssetClass::trading_seconds_per_bar),
    /// which answers how much *trading* a bar contains — 6.5 hours for a US
    /// equity `1d` bar, not 24. Which one a caller wants depends on what it is
    /// measuring: annualizing a return uses trading time, because that is when
    /// the return happened; accruing **interest** uses calendar time, because a
    /// broker charges over the weekend. Reaching for the wrong one under-charges
    /// equity margin interest by nearly 4x.
    pub fn calendar_seconds_per_bar(self) -> u64 {
        match self {
            Frequency::Minute(n) => 60 * n as u64,
            Frequency::Hour(n) => 3_600 * n as u64,
            Frequency::Day(n) => 86_400 * n as u64,
            Frequency::Week(n) => 604_800 * n as u64,
            Frequency::Month(n) => 2_592_000 * n as u64,
        }
    }

    /// A stable rank per variant, used as a tie-breaker when two cadences
    /// have the same `seconds_per_bar` (`Hour(24)` and `Day(1)`, say). Finer
    /// units rank lower so they sort first — the derived `PartialEq` keeps
    /// the two cases distinct, and the `Ord` contract (equal iff `PartialEq`
    /// says so) is preserved.
    fn variant_rank(self) -> u8 {
        match self {
            Frequency::Minute(_) => 0,
            Frequency::Hour(_) => 1,
            Frequency::Day(_) => 2,
            Frequency::Week(_) => 3,
            Frequency::Month(_) => 4,
        }
    }

    /// The canonical `N<unit>` token — the round-trip of [`FromStr`].
    pub fn as_token(self) -> String {
        match self {
            Frequency::Minute(n) => format!("{n}m"),
            Frequency::Hour(n) => format!("{n}h"),
            Frequency::Day(n) => format!("{n}d"),
            Frequency::Week(n) => format!("{n}w"),
            Frequency::Month(n) => format!("{n}M"),
        }
    }
}

impl Ord for Frequency {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.calendar_seconds_per_bar()
            .cmp(&other.calendar_seconds_per_bar())
            .then_with(|| self.variant_rank().cmp(&other.variant_rank()))
    }
}

impl PartialOrd for Frequency {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Frequency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_token())
    }
}

impl FromStr for Frequency {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        // Split at the first alphabetic byte: the numeric prefix is the
        // multiplier, the suffix is the unit. Reject anything else (empty
        // number, missing unit, extra tail).
        let split = s
            .find(|c: char| c.is_alphabetic())
            .ok_or_else(|| format!("`{s}`: expected `N<unit>` (unit m/h/d/w/M)"))?;
        let (num, unit) = s.split_at(split);
        let n: u32 = num
            .parse()
            .map_err(|_| format!("`{s}`: `{num}` is not a positive integer multiplier"))?;
        if n == 0 {
            return Err(format!("`{s}`: multiplier must be > 0"));
        }
        match unit {
            "m" => Ok(Frequency::Minute(n)),
            "h" => Ok(Frequency::Hour(n)),
            "d" => Ok(Frequency::Day(n)),
            "w" => Ok(Frequency::Week(n)),
            "M" => Ok(Frequency::Month(n)),
            other => Err(format!(
                "`{s}`: unknown unit `{other}`, expected one of m/h/d/w/M"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole `i64` millisecond range is a valid `Timestamp`; only part of
    /// it is a valid `OffsetDateTime`. The out-of-range half used to abort.
    #[test]
    fn an_unrepresentable_stamp_reads_none_rather_than_aborting() {
        // Year 9999-12-31 23:59:59.999 is the last representable millisecond.
        let last = 253_402_300_799_999i64;
        assert!(Timestamp(last).to_datetime().is_some());
        assert!(Timestamp(last + 1).to_datetime().is_none());
        // The shape that actually reaches here: a nanosecond epoch column read
        // as milliseconds.
        assert!(Timestamp(1_704_067_200_000_000_000).to_datetime().is_none());
        assert!(Timestamp(i64::MAX).to_datetime().is_none());
        assert!(Timestamp(i64::MIN).to_datetime().is_none());
        // Ordinary stamps, and the pre-epoch ones a long equity history
        // carries, are unaffected.
        assert!(Timestamp(0).to_datetime().is_some());
        assert!(Timestamp(1_704_067_200_000).to_datetime().is_some());
        assert!(Timestamp(-2_208_988_800_000).to_datetime().is_some());
    }

    #[test]
    fn a_representable_stamp_round_trips_through_the_datetime() {
        for ms in [0i64, 1_704_067_200_000, -2_208_988_800_000, 1_234_567_891] {
            let dt = Timestamp(ms).to_datetime().expect("representable");
            assert_eq!(Timestamp::from_datetime(dt).0, ms);
        }
    }

    /// The cadence grammar is `N<unit>` with a positive multiplier and one of
    /// five units — everything else is bad input, reported rather than guessed
    /// at.
    #[test]
    fn frequency_parsing_refuses_everything_that_is_not_a_cadence() {
        for good in ["1m", "5m", "4h", "1d", "2w", "1M", " 15m "] {
            let f: Frequency = good.parse().expect(good);
            assert_eq!(f.as_token(), good.trim());
        }
        for bad in ["", "m", "0m", "-5m", "5", "5min", "5M5", "1s", "1y", "1.5h"] {
            assert!(bad.parse::<Frequency>().is_err(), "accepted `{bad}`");
        }
    }

    /// Ordered by duration, not by variant tag — the reason `Ord` is
    /// hand-written. The tie-break keeps `Hour(24)` and `Day(1)` distinct
    /// without breaking the `Ord`/`Eq` contract.
    #[test]
    fn cadences_order_by_duration_across_units() {
        assert!(Frequency::Minute(120) > Frequency::Hour(1));
        assert!(Frequency::Hour(25) > Frequency::Day(1));
        assert!(Frequency::Day(8) > Frequency::Week(1));
        assert!(Frequency::Week(5) > Frequency::Month(1));
        // Equal duration, different unit: ordered but not equal.
        assert_ne!(Frequency::Hour(24), Frequency::Day(1));
        assert!(Frequency::Hour(24) < Frequency::Day(1));
        assert_eq!(
            Frequency::Hour(24).cmp(&Frequency::Hour(24)),
            std::cmp::Ordering::Equal
        );
    }
}
