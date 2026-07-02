//! Remote candle providers.
//!
//! This module is fugazi's first step outside the pure indicator/backtest core:
//! it introduces a generic [`CandleSource`] trait and one built-in
//! implementation ([`Binance`]) that fetches OHLCV bars from a live HTTP API.
//!
//! The pieces are:
//!
//! * [`CandleSource`] — the async trait every provider implements.
//! * [`Timestamp`] — a flat i64-millis UTC epoch stamp. Kept `Copy` and free of
//!   the `time` crate in its ABI so the trait signature stays simple.
//! * [`TimedCandle`] — a [`crate::Candle`] paired with its open time; the
//!   value type providers return. The pure `Candle` stays timeless so the
//!   indicator layer is unaffected.
//! * [`Interval`] — the bar cadence, an enum because providers advertise a
//!   discrete vocabulary of tokens. Constructed directly (`Interval::Day(1)`,
//!   `Interval::Hour(4)`, …); string parsing is a caller-side concern.
//! * [`SourceError`] — a single unified enum, so a caller that fans errors in
//!   from several providers doesn't need per-impl error plumbing.
//!
//! **Everything here takes objects/enums, not strings.** The CLI's `get`
//! subcommand and the Python bindings do their own string parsing before
//! calling into this layer.
//!
//! Example:
//!
//! ```no_run
//! use fugazi::sources::{Binance, CandleSource, Interval, Timestamp};
//!
//! # async fn demo() -> Result<(), fugazi::sources::SourceError> {
//! let b = Binance::new();
//! let since = Timestamp(1_704_067_200_000); // 2024-01-01 UTC
//! let rows = b.candles("BTCEUR", Interval::Day(1), since, None).await?;
//! for row in &rows {
//!     println!("{:?} {}", row.time, row.candle.close);
//! }
//! # Ok(()) }
//! ```

pub mod binance;
pub mod yahoo;

use std::fmt;
use std::future::Future;

use crate::types::Candle;

pub use binance::Binance;
pub use yahoo::Yahoo;

/// A UTC millisecond timestamp (Unix epoch).
///
/// Kept as a flat `i64` on purpose: it matches Binance's native representation,
/// stays `Copy`, and keeps `time::OffsetDateTime` out of the [`CandleSource`]
/// trait's public ABI (callers that want a datetime can call
/// [`Timestamp::to_datetime`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(pub i64);

impl Timestamp {
    /// The current UTC time, in milliseconds since the Unix epoch.
    pub fn now() -> Self {
        Self::from_datetime(time::OffsetDateTime::now_utc())
    }

    /// Convert a `time::OffsetDateTime` to a millisecond epoch stamp.
    pub fn from_datetime(dt: time::OffsetDateTime) -> Self {
        let nanos = dt.unix_timestamp_nanos();
        Self((nanos / 1_000_000) as i64)
    }

    /// Reconstruct a `time::OffsetDateTime` at UTC from this millisecond stamp.
    pub fn to_datetime(self) -> time::OffsetDateTime {
        let nanos = (self.0 as i128) * 1_000_000;
        time::OffsetDateTime::from_unix_timestamp_nanos(nanos)
            .expect("i64 millis fits in OffsetDateTime range")
    }
}

/// A [`Candle`] paired with its bar-open [`Timestamp`].
///
/// This is deliberately *not* a `Candle` field: keeping the pure `Candle`
/// timeless lets the incremental indicator core stay unchanged, and lets a
/// bar-stream driver decide for itself whether to persist times.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TimedCandle {
    /// Bar-open time, UTC millis.
    pub time: Timestamp,
    /// The candle.
    pub candle: Candle,
}

/// Bar cadence advertised by a provider.
///
/// An enum, not a plain [`std::time::Duration`], because providers speak a
/// discrete vocabulary and must map the cadence to their own tokens.
/// Constructed directly (`Interval::Day(1)`, `Interval::Hour(4)`, …) — the
/// library deliberately does not offer a string parser, since that concern
/// belongs to the CLI / bindings layer, not the fetching API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Interval {
    Minute(u32),
    Hour(u32),
    Day(u32),
    Week(u32),
    Month(u32),
}

impl Interval {
    /// The Binance-style token for this interval (`"1d"`, `"4h"`, `"1M"`, …).
    pub fn as_token(self) -> String {
        match self {
            Interval::Minute(n) => format!("{n}m"),
            Interval::Hour(n) => format!("{n}h"),
            Interval::Day(n) => format!("{n}d"),
            Interval::Week(n) => format!("{n}w"),
            Interval::Month(n) => format!("{n}M"),
        }
    }

    /// The interval's duration in milliseconds.
    ///
    /// `Week` uses seven 86_400_000-ms days. `Month` is **approximate** at 30
    /// days — real calendar months vary from 28 to 31 days, so callers that
    /// need exact month lengths should compute against actual dates.
    pub fn duration_ms(self) -> i64 {
        const MIN: i64 = 60_000;
        const HOUR: i64 = 60 * MIN;
        const DAY: i64 = 24 * HOUR;
        match self {
            Interval::Minute(n) => (n as i64) * MIN,
            Interval::Hour(n) => (n as i64) * HOUR,
            Interval::Day(n) => (n as i64) * DAY,
            Interval::Week(n) => (n as i64) * 7 * DAY,
            Interval::Month(n) => (n as i64) * 30 * DAY,
        }
    }
}

impl fmt::Display for Interval {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_token())
    }
}

/// One unified error type for every [`CandleSource`] implementation. Providers
/// that need their own richer error data can nest it inside the `Decode`
/// variant.
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),
    #[error("http {status}: {body}")]
    Http { status: u16, body: String },
    #[error("decode: {0}")]
    Decode(String),
    #[error("rate limited (retry after {retry_after_ms}ms)")]
    RateLimited { retry_after_ms: u64 },
    #[error("unknown symbol: {0}")]
    UnknownSymbol(String),
    #[error("unsupported interval: {0:?}")]
    UnsupportedInterval(Interval),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// A remote candle provider.
///
/// Implementations fetch OHLCV bars for `symbol` in the given `interval`,
/// covering `[since, until)` (where `until = None` means "up to now"), and
/// return them ascending by [`TimedCandle::time`]. Pagination, rate-limiting,
/// and API-specific errors are the implementation's concern.
///
/// The trait uses an edition-2024 explicit-return-position `impl Future`
/// signature (rather than `async fn`) so callers can name the future's bounds
/// (`Send`) at the call site without any macros.
pub trait CandleSource: Send + Sync {
    /// The provider's short, lowercase name (e.g. `"binance"`).
    fn name(&self) -> &'static str;

    /// Fetch candles for `symbol` in `[since, until)` — `since` inclusive,
    /// `until` exclusive; `until = None` means "up to now".
    fn candles(
        &self,
        symbol: &str,
        interval: Interval,
        since: Timestamp,
        until: Option<Timestamp>,
    ) -> impl Future<Output = Result<Vec<TimedCandle>, SourceError>> + Send;
}
