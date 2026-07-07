//! Remote candle providers.
//!
//! This module is fugazi's first step outside the pure indicator/backtest core:
//! it introduces a generic [`CandleSource`] trait and one built-in
//! implementation ([`Binance`]) that fetches OHLCV bars from a live HTTP API.
//!
//! The pieces are:
//!
//! * [`CandleSource`] — the async trait every provider implements. Fetches
//!   yield **`Vec<Atom>`**: every returned atom carries `time: Some(_)` and,
//!   for providers that expose them, per-bar overlay values behind a
//!   provider-defined [`Schema`]. Downstream consumers (calendar indicators,
//!   the `!get { key }` overlay reference) then compose naturally.
//! * [`Timestamp`] — re-exported from [`crate::types`]; a flat i64-millis UTC
//!   epoch stamp, `Copy`, with `time`-crate helpers on the pure core.
//! * [`Interval`] — the bar cadence, an enum because providers advertise a
//!   discrete vocabulary of tokens. Constructed directly (`Interval::Day(1)`,
//!   `Interval::Hour(4)`, …); string parsing is a caller-side concern.
//! * [`SourceError`] — a single unified enum, so a caller that fans errors in
//!   from several providers doesn't need per-impl error plumbing.
//! * [`schema_of`] — the "which side channel is this atom stream carrying?"
//!   helper. Every atom in a fetch shares one `Arc<Schema>`; this picks it
//!   off the first atom that has overlays and defaults to [`Schema::empty()`]
//!   for a stream that carries none.
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
//! let rows = b.atoms("BTCEUR", Interval::Day(1), since, None).await?;
//! for row in &rows {
//!     println!("{:?} {}", row.time, row.candle.close);
//! }
//! # Ok(()) }
//! ```

pub mod binance;
pub mod yahoo;

use std::fmt;
use std::future::Future;
use std::sync::Arc;

use crate::types::{Atom, Schema};
pub use crate::types::Timestamp;

pub use binance::Binance;
pub use yahoo::Yahoo;

/// The shared [`Schema`] carried by an atom stream, or [`Schema::empty()`] if
/// none of the atoms bind an [`OverlayInfo`](crate::OverlayInfo).
///
/// Every atom in one fetch shares the same `Arc<Schema>` (the provider builds
/// it once and clones the pointer into each atom's overlay side channel), so
/// a consumer only needs to peek at any timestamped atom to know what fields
/// the batch carries. Consumed by [`crate::cli::backtest::schema_from_atoms`]
/// and by the `fugazi get` overlay pipeline to decide the vocabulary
/// available to `!get { key }` references.
pub fn schema_of(atoms: &[Atom]) -> Arc<Schema> {
    atoms
        .iter()
        .find_map(|a| a.overlays.as_ref().map(|o| o.schema().clone()))
        .unwrap_or_else(Schema::empty)
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
    #[error("{provider} does not support {operation}")]
    Unsupported {
        operation: &'static str,
        provider: &'static str,
    },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// A remote candle provider.
///
/// Implementations fetch OHLCV bars for `symbol` in the given `interval`,
/// covering `[since, until)` (where `until = None` means "up to now"), and
/// return them as [`Atom`]s ascending by [`Atom::time`] — every returned
/// atom carries `time: Some(_)` and, when the provider exposes them, per-bar
/// overlay values behind a provider-defined [`Schema`] (Binance's
/// `quote_volume` / `n_trades` / …; Yahoo's `adj_close`). One `Arc<Schema>`
/// is shared across every atom in a fetch; use [`schema_of`] to pick it off
/// the returned slice. Pagination, rate-limiting, and API-specific errors
/// are the implementation's concern.
///
/// The trait uses an edition-2024 explicit-return-position `impl Future`
/// signature (rather than `async fn`) so callers can name the future's bounds
/// (`Send`) at the call site without any macros.
pub trait CandleSource: Send + Sync {
    /// The provider's short, lowercase name (e.g. `"binance"`).
    fn name(&self) -> &'static str;

    /// Fetch atoms for `symbol` in `[since, until)` — `since` inclusive,
    /// `until` exclusive; `until = None` means "up to now".
    fn atoms(
        &self,
        symbol: &str,
        interval: Interval,
        since: Timestamp,
        until: Option<Timestamp>,
    ) -> impl Future<Output = Result<Vec<Atom>, SourceError>> + Send;

    /// Enumerate every symbol this provider currently exposes. The default
    /// implementation returns [`SourceError::Unsupported`], since a canonical
    /// "list every symbol" endpoint is not universal — Binance advertises its
    /// entire spot vocabulary through `/api/v3/exchangeInfo`, but Yahoo
    /// Finance (and most retail equity APIs) offer no such call.
    fn tickers(&self) -> impl Future<Output = Result<Vec<String>, SourceError>> + Send {
        let provider = self.name();
        async move {
            Err(SourceError::Unsupported {
                operation: "ticker enumeration",
                provider,
            })
        }
    }
}
