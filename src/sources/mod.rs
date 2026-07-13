//! Remote data providers.
//!
//! This module is fugazi's first step outside the pure indicator/backtest core:
//! it introduces the provider traits and their built-in implementations, which
//! fetch from live HTTP APIs.
//!
//! **There are two provider traits, split by the shape of what they return** —
//! not one trait with a capability flag. See [`OverlaySource`] for why that
//! split is load-bearing rather than cosmetic.
//!
//! The pieces are:
//!
//! * [`CandleSource`] — providers of OHLCV bars ([`Binance`], [`Yahoo`]).
//!   Fetches yield **`Vec<Atom>`**: every returned atom carries `time: Some(_)`
//!   and, for providers that expose them, per-bar overlay values behind a
//!   provider-defined [`Schema`]. Downstream consumers (calendar indicators,
//!   the `!get { key }` overlay reference) then compose naturally.
//! * [`OverlaySource`] — providers of per-bar side-channel columns with **no
//!   OHLCV** (market capitalisation, supply, open interest, funding rates).
//!   Fetches yield **`Vec<OverlayRow>`**: a timestamp plus that bar's values,
//!   and deliberately no candle. Joined onto a price series by `(symbol, time)`
//!   downstream.
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
pub mod coingecko;
pub mod coinmarketcap;
pub mod yahoo;

use std::fmt;
use std::future::Future;
use std::sync::Arc;

use crate::types::{Atom, OverlayInfo, Schema};
pub use crate::types::Timestamp;

pub use binance::Binance;
pub use coingecko::CoinGecko;
pub use coinmarketcap::CoinMarketCap;
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

/// One timestamped row from an [`OverlaySource`]: a bar-open [`Timestamp`] and
/// that bar's overlay values bound to the provider's [`Schema`].
///
/// Deliberately **not** an [`Atom`]. An `Atom` carries a non-optional
/// [`Candle`](crate::Candle), and the whole point of an `OverlaySource` is that
/// it has no OHLCV to put there — a synthesised zero/NaN candle would flow
/// straight into `Current::close()` and into the wallet's mark-to-market
/// (`Wallet::update` prices a symbol from the bar it is fed). Keeping the row
/// candle-less makes that mistake unrepresentable rather than merely
/// discouraged.
/// Rows carry no symbol: like [`CandleSource::atoms`], [`OverlaySource::overlays`]
/// is a per-symbol call, so the symbol is the *argument*, not a field. A caller
/// fetching several symbols tags the rows on its own side (the CLI's `get`
/// pipeline does exactly this, then sorts by `(time, symbol, freq)`).
#[derive(Debug, Clone)]
pub struct OverlayRow {
    /// Bar-open timestamp, aligned to the requested [`Interval`]'s boundary.
    pub time: Timestamp,
    /// This bar's values, in the provider [`Schema`]'s column order.
    pub overlays: OverlayInfo,
}

/// Equality is **by `time`**, exactly as it is for [`Atom`] — two rows are the
/// same row iff they describe the same bar. The overlay payload is deliberately
/// not compared: it is a bag of `f64`s that may contain `NaN` (a missing cell),
/// which would make equality neither reflexive nor useful.
impl PartialEq for OverlayRow {
    fn eq(&self, other: &Self) -> bool {
        self.time == other.time
    }
}

impl Eq for OverlayRow {}

/// Chronological ordering, matching [`Atom`]'s: rows sort by bar-open
/// [`Timestamp`], so a merged slice of rows from several fetches sorts into run
/// order and dedups by bar without a custom key.
impl PartialOrd for OverlayRow {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OverlayRow {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.time.cmp(&other.time)
    }
}

/// A remote provider of **per-bar side-channel data with no OHLCV** — market
/// capitalisation, circulating supply, open interest, funding rates, sentiment
/// scores, and anything else that is a property of an instrument at a point in
/// time rather than a price bar.
///
/// The sibling of [`CandleSource`], and deliberately a separate trait rather
/// than a capability flag on it: the two return different shapes ([`OverlayRow`]
/// versus [`Atom`]), and a provider that implements both is free to do so.
///
/// **How the data is consumed.** An `OverlaySource` produces columns, not a
/// tradeable series. The intended flow is to fetch the overlays into their own
/// CSV, and join them onto a price series from a [`CandleSource`] by
/// `(symbol, time)` — which is exactly what the CLI's `--series` dataframe does:
///
/// ```text
/// fugazi get binance:BTCUSDT[1d]                      -o prices.csv
/// fugazi get coingecko:BTCUSDT=bitcoin[1d]            -o caps.csv
/// fugazi run @strategy.yml -s @prices.csv -s @caps.csv -o out/
/// ```
///
/// The joined columns land in the run's [`Schema`] and are read from a strategy
/// spec with `!get { key: market_cap }`. The `OUTPUT=QUERY` form above is what
/// makes the join key line up: the row is *fetched* under the provider's own
/// identifier (CoinGecko's `bitcoin`) and *emitted* under the symbol the price
/// series uses (`BTCUSDT`).
pub trait OverlaySource: Send + Sync {
    /// The provider's short, lowercase name (e.g. `"cg"`).
    fn name(&self) -> &'static str;

    /// The overlay [`Schema`] every row from this provider binds to. Stable for
    /// the provider's lifetime, so a caller can build its output columns before
    /// fetching anything.
    fn schema(&self) -> Arc<Schema>;

    /// Fetch overlay rows for `symbol` in `[since, until)` — `since` inclusive,
    /// `until` exclusive; `until = None` means "up to now". Rows come back
    /// ascending by [`OverlayRow::time`], each timestamp aligned to the
    /// requested `interval`'s bar-open boundary so they join cleanly against a
    /// [`CandleSource`] stream of the same cadence.
    fn overlays(
        &self,
        symbol: &str,
        interval: Interval,
        since: Timestamp,
        until: Option<Timestamp>,
    ) -> impl Future<Output = Result<Vec<OverlayRow>, SourceError>> + Send;

    /// Enumerate every symbol this provider exposes. Same contract (and same
    /// [`SourceError::Unsupported`] default) as [`CandleSource::tickers`].
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{OverlayValue, Real};

    fn row(time: i64, price: Real) -> OverlayRow {
        let schema = {
            let mut b = Schema::builder();
            b.add_real("price");
            b.finish()
        };
        OverlayRow {
            time: Timestamp(time),
            overlays: OverlayInfo::new(schema, vec![OverlayValue::Real(price)]),
        }
    }

    #[test]
    fn overlay_rows_compare_and_sort_by_time_only() {
        // Same bar, different payload — still the same row, exactly as for `Atom`.
        assert_eq!(row(100, 1.0), row(100, 999.0));
        assert_ne!(row(100, 1.0), row(200, 1.0));

        // A payload carrying a missing cell (NaN) must not poison equality.
        assert_eq!(row(100, Real::NAN), row(100, Real::NAN));

        let mut rows = [row(300, 3.0), row(100, 1.0), row(200, 2.0)];
        rows.sort();
        let times: Vec<i64> = rows.iter().map(|r| r.time.0).collect();
        assert_eq!(times, vec![100, 200, 300]);
    }
}
