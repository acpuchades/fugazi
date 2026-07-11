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

    /// Reconstruct a `time::OffsetDateTime` at UTC from this millisecond stamp.
    pub fn to_datetime(self) -> ::time::OffsetDateTime {
        let nanos = (self.0 as i128) * 1_000_000;
        ::time::OffsetDateTime::from_unix_timestamp_nanos(nanos)
            .expect("i64 millis fits in OffsetDateTime range")
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
    /// The approximate seconds a bar of this cadence spans, using calendar
    /// conventions (30-day month, 7-day week). Used as the primary total-
    /// order key by [`Ord`] so cadences sort by duration regardless of
    /// variant, which keeps `Frequency::Minute(120) > Frequency::Hour(1)` (a
    /// derived `Ord` would order them lexicographically by variant tag and
    /// get it wrong).
    fn seconds_per_bar(self) -> u64 {
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
        self.seconds_per_bar()
            .cmp(&other.seconds_per_bar())
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
