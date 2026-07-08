//! Asset-class + bar-frequency defaults for the annualization calendar.
//!
//! `metrics.yml` reports annualized figures (Sharpe, Sortino, CAGR,
//! annualized vol) by scaling per-bar moments by `bars_per_year`. That
//! constant depends on the market the strategy trades *and* the bar cadence
//! it consumes: a daily-bar equity strategy uses 252 (US trading days),
//! whereas the same daily bars on a 24/7 crypto series use 365. Getting it
//! wrong doesn't fail the run — it silently misreports the annualized block.
//!
//! Rather than force every run to spell out `--bars-per-year`, the CLI takes
//! two orthogonal shortcuts that compose:
//!
//! * `--stocks` / `--forex` / `--crypto` — the trading calendar. Determines
//!   how many trading days/hours per year the market is open (equities ~252
//!   days × 6.5h, forex ~260 weekdays × 24h, crypto 365 × 24h).
//! * `-f, --frequency <CODE>` — the bar cadence (`1m`, `5m`, `15m`, `30m`,
//!   `1h`, `4h`, `1d`, `1w`, `1M`, or any `N<unit>` in the same alphabet).
//!
//! Together they resolve to a bars-per-year figure via
//! [`AssetClass::bars_per_year`]. An explicit `--bars-per-year` always
//! overrides. Extend this module (not the CLI arg block) when new class-level
//! defaults are added — commissions, slippage models, tick sizes — so the
//! shortcut group stays the single place a "market" is described.
//!
//! When neither `--bars-per-year` nor `-f/--frequency` is given, callers can
//! call [`detect_frequency_from_atoms`] on the loaded atoms to guess the
//! cadence from the median inter-bar gap — snapped to the nearest well-known
//! [`Frequency`] variant. The atoms' `time` field is populated at load by the
//! [`--series` loader](crate::data), so no re-parse of the time-column
//! strings is needed.
//!
//! `--bars-per-year` itself is repeatable and each entry may carry a
//! `SYMBOL[FREQ]:` scope prefix — the `[`crate::costs`] grammar — so a
//! preset file can pre-declare per-series overrides (e.g. `BTC[1h]:8760`
//! alongside `AAPL[1d]:252`). [`pick_bars_per_year`] resolves which entry
//! (if any) wins for a run's `(symbol, effective_freq)`; no match falls
//! through to the class × frequency calendar (see [`resolve`]).

use std::num::NonZeroUsize;
use std::str::FromStr;

use anyhow::{Result, bail};
use fugazi::prelude::*;
use fugazi::sources::Interval;
use time::format_description::well_known::Rfc3339;
use time::macros::format_description;

/// A trading calendar shortcut. Determines the annualization denominators —
/// how many trading days a year the market is open, and how many hours per
/// trading day.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssetClass {
    /// US-equity convention: 252 trading days a year, 6.5-hour trading day.
    Stocks,
    /// Forex convention: ~260 weekdays a year, 24-hour trading day
    /// (Sun-open / Fri-close counted as continuous).
    Forex,
    /// 24/7 markets (crypto): 365 days a year, 24-hour day.
    Crypto,
}

impl AssetClass {
    /// Trading days the market is open per calendar year.
    pub fn trading_days_per_year(self) -> Real {
        match self {
            AssetClass::Stocks => 252.0,
            AssetClass::Forex => 260.0,
            AssetClass::Crypto => 365.0,
        }
    }

    /// Trading hours per trading day (equities are ~6.5h; forex/crypto 24h).
    pub fn trading_hours_per_day(self) -> Real {
        match self {
            AssetClass::Stocks => 6.5,
            AssetClass::Forex | AssetClass::Crypto => 24.0,
        }
    }

    /// Trading hours the market is open per calendar year.
    pub fn trading_hours_per_year(self) -> Real {
        self.trading_days_per_year() * self.trading_hours_per_day()
    }

    /// Trading seconds a bar of `freq` spans on this calendar. Sub-daily
    /// cadences (`Minute`, `Hour`) are already in trading time because bars
    /// only exist during trading hours — the calendar `seconds_per_bar` is
    /// exact. Daily and longer cadences scale to trading time: `Day(n)` is
    /// `n · trading_hours_per_day · 3600`, `Week(n)` is
    /// `n · trading_seconds_per_year / 52`, `Month(n)` is
    /// `n · trading_seconds_per_year / 12`. Consumed by
    /// [`WindowSpec::resolve`] to convert a duration and a bar cadence into a
    /// bar count without averaging trading and closed periods into one
    /// calendar rate.
    pub fn trading_seconds_per_bar(self, freq: Frequency) -> Real {
        let trading_secs_per_year =
            self.trading_days_per_year() * self.trading_hours_per_day() * 3_600.0;
        match freq {
            Frequency::Minute(n) => n as Real * 60.0,
            Frequency::Hour(n) => n as Real * 3_600.0,
            Frequency::Day(n) => n as Real * self.trading_hours_per_day() * 3_600.0,
            Frequency::Week(n) => n as Real * trading_secs_per_year / 52.0,
            Frequency::Month(n) => n as Real * trading_secs_per_year / 12.0,
        }
    }

    /// The `bars_per_year` figure for this calendar with bars of `freq` each.
    /// Sub-daily bars scale by trading *hours* per year, day-and-up bars scale
    /// by trading *days*. Weekly/monthly clamp to the calendar rather than
    /// trading-day arithmetic (52 weeks / 12 months a year regardless of
    /// class), which matches how those cadences are reported in practice.
    pub fn bars_per_year(self, freq: Frequency) -> Real {
        match freq {
            Frequency::Minute(n) => self.trading_hours_per_year() * 60.0 / n as Real,
            Frequency::Hour(n) => self.trading_hours_per_year() / n as Real,
            Frequency::Day(n) => self.trading_days_per_year() / n as Real,
            Frequency::Week(n) => 52.0 / n as Real,
            Frequency::Month(n) => 12.0 / n as Real,
        }
    }
}

// The bar-cadence type itself lives in the core (`fugazi::types::Frequency`) —
// it's the same alphabet used by `!pick { freq }` and `Snapshot<Selector>`.
pub use fugazi::Frequency;

/// Parse a Binance-style interval token (`1m`, `5m`, `1h`, `4h`, `1d`, `1w`,
/// `1M`) into an [`Interval`]. Case-sensitive on the unit letter: `m` = minute,
/// `M` = month. The same `N<unit>` alphabet as [`Frequency::from_str`], but
/// yields the sources-layer [`Interval`] used by the remote candle providers.
pub(crate) fn parse_interval(s: &str) -> Result<Interval> {
    let s = s.trim();
    if s.is_empty() {
        bail!("empty interval token");
    }
    let (num, unit) = s.split_at(s.len() - 1);
    let n: u32 = if num.is_empty() {
        1
    } else {
        num.parse().map_err(|_| anyhow::anyhow!("bad interval {s:?}"))?
    };
    if n == 0 {
        bail!("interval {s:?}: multiplier must be positive");
    }
    match unit {
        "m" => Ok(Interval::Minute(n)),
        "h" => Ok(Interval::Hour(n)),
        "d" => Ok(Interval::Day(n)),
        "w" => Ok(Interval::Week(n)),
        "M" => Ok(Interval::Month(n)),
        _ => bail!("interval {s:?}: unknown unit letter {unit:?}"),
    }
}

/// A `-w/--windowed` value: either an explicit bar count (`10`, `252`) or a
/// duration in the [`Frequency`] alphabet (`1d`, `1w`, `1M`, `4h`) that
/// resolves to a bar count against the run's trading calendar. The duration
/// form frees a preset from recomputing a bar count for every timeframe.
///
/// Resolution is a closed-form ratio in trading time (see
/// [`AssetClass::trading_seconds_per_bar`]):
/// `bars = win.trading_seconds / bar_freq.trading_seconds`. That naturally
/// accounts for closed sessions — `-w 1d` on hourly equities picks 7 bars
/// (one 6.5-hour trading day, not 24), `-w 1w` on daily equities picks 5
/// (M-F), on continuous crypto 7. Rounded to the nearest positive integer.
///
/// The duration form is deliberately strict: it demands both an
/// [`AssetClass`] (`--stocks` / `--forex` / `--crypto`) and a bar cadence
/// (`-f/--frequency`, or auto-detected from the input's `time` column). A
/// silent default in either dimension would change the resolved bar count in
/// a way that's hard to notice — passing hourly BTC as if it were 6.5h/day
/// equities, or hourly equities as if they ran 24/7. Missing either is a
/// hard error; a window shorter than one bar of the effective cadence is
/// also a hard error.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum WindowSpec {
    /// An explicit bar count — the classic `-w N` shape.
    Bars(NonZeroUsize),
    /// A duration whose bar count is derived from the trading calendar.
    Duration(Frequency),
}

impl std::fmt::Display for WindowSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WindowSpec::Bars(n) => write!(f, "{n}"),
            WindowSpec::Duration(freq) => f.write_str(&format_freq(*freq)),
        }
    }
}

impl FromStr for WindowSpec {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s.is_empty() {
            return Err("empty `--windowed` value".to_string());
        }
        // Trailing alphabetic byte → duration form (same alphabet as
        // `Frequency::from_str`). Otherwise plain bar count.
        let last = s.as_bytes()[s.len() - 1];
        if last.is_ascii_alphabetic() {
            let freq = Frequency::from_str(s)
                .map_err(|e| format!("`{s}`: {e} (or pass a plain bar count)"))?;
            Ok(WindowSpec::Duration(freq))
        } else {
            let n: NonZeroUsize = s.parse().map_err(|_| {
                format!(
                    "`{s}`: expected a positive bar count or a duration like `1d`/`1w`/`1M`"
                )
            })?;
            Ok(WindowSpec::Bars(n))
        }
    }
}

impl WindowSpec {
    /// Resolve to a concrete bar count.
    ///
    /// [`Bars`](Self::Bars) is the identity (needs neither `bar_freq` nor
    /// `class`). [`Duration`](Self::Duration) computes the closed-form ratio
    /// `bars = win.trading_seconds(class) / bar_freq.trading_seconds(class)`
    /// via [`AssetClass::trading_seconds_per_bar`], rounded to the nearest
    /// positive integer.
    ///
    /// Errors:
    /// * `Duration` without a `class` — silent default would mis-interpret
    ///   sub-daily durations on markets whose trading day isn't 24 hours;
    ///   emitted before checking `bar_freq` so the fix is surfaced first.
    /// * `Duration` without a `bar_freq` — the caller must pass
    ///   `-f/--frequency` or supply an input with a parseable `time`
    ///   column so the cadence can be auto-detected.
    /// * `Duration` shorter than one bar — would round to zero.
    pub fn resolve(
        &self,
        bar_freq: Option<Frequency>,
        class: Option<AssetClass>,
    ) -> Result<NonZeroUsize, String> {
        match *self {
            WindowSpec::Bars(n) => Ok(n),
            WindowSpec::Duration(win) => {
                let class = class.ok_or_else(|| {
                    format!(
                        "`-w {}`: duration form requires an explicit trading calendar — pass `--stocks`, `--forex`, or `--crypto`",
                        format_freq(win),
                    )
                })?;
                let bar_freq = bar_freq.ok_or_else(|| {
                    format!(
                        "`-w {}`: no bar cadence known — pass `-f/--frequency` or provide input with a parseable `time` column",
                        format_freq(win),
                    )
                })?;
                let bars = class.trading_seconds_per_bar(win)
                    / class.trading_seconds_per_bar(bar_freq);
                let n = bars.round() as usize;
                NonZeroUsize::new(n).ok_or_else(|| {
                    format!(
                        "`-w {}`: window shorter than one bar of the input's cadence",
                        format_freq(win),
                    )
                })
            }
        }
    }
}

/// Render a [`Frequency`] as its canonical CLI code (`1m`, `4h`, `1d`, `1w`,
/// `1M`) — the inverse of [`Frequency::from_str`].
fn format_freq(f: Frequency) -> String {
    match f {
        Frequency::Minute(n) => format!("{n}m"),
        Frequency::Hour(n) => format!("{n}h"),
        Frequency::Day(n) => format!("{n}d"),
        Frequency::Week(n) => format!("{n}w"),
        Frequency::Month(n) => format!("{n}M"),
    }
}

/// Resolve `bars_per_year` from the CLI's three inputs in priority order:
///
/// 1. an explicit `--bars-per-year <N>` — always wins;
/// 2. `--<class> -f <freq>` — the derived value from the calendar × cadence;
/// 3. one side of the pair alone — the missing side falls back to a sensible
///    default (class = [`AssetClass::Stocks`], freq = daily);
/// 4. nothing set — returns 252, matching the legacy default.
pub fn resolve(
    explicit: Option<Real>,
    class: Option<AssetClass>,
    freq: Option<Frequency>,
) -> Real {
    if let Some(v) = explicit {
        return v;
    }
    let class = class.unwrap_or(AssetClass::Stocks);
    let freq = freq.unwrap_or(Frequency::Day(1));
    class.bars_per_year(freq)
}

/// Best-effort auto-detection of a bar cadence from a series' `time` column.
///
/// Parses each string with a small vocabulary of common shapes — RFC 3339
/// (`2024-01-01T00:00:00Z`), date-only (`2024-01-01`), naive datetime
/// (`2024-01-01 00:00:00`), or an integer Unix epoch in seconds (or
/// milliseconds, autodetected from the magnitude) — takes the median positive
/// gap between consecutive parsed times, and snaps it to the nearest named
/// [`Frequency`] in log space (a 3-minute gap picks `5m`, a 10-day gap picks
/// `1w`). Returns `None` when fewer than two times parse or every gap is
/// non-positive — the caller should fall through to the static default (see
/// [`resolve`]).
///
/// The caller is expected to partition its input by `(symbol, freq column
/// value)` and detect *per group*, so a frame mixing several cadences of the
/// same symbol doesn't average their gaps into a nonsense median.
pub fn detect_frequency_from_atoms<'a>(
    atoms: impl IntoIterator<Item = &'a Atom>,
) -> Option<Frequency> {
    detect_frequency_from_millis(atoms.into_iter().filter_map(|a| a.time.map(|t| t.0)))
}

/// Snap the median gap to the nearest named [`Frequency`]. Shared core of
/// [`detect_frequency_from_atoms`]; also directly consumed by tests that want
/// to exercise the string-parse vocabulary via [`parse_time_to_millis`]
/// without constructing atoms.
pub(crate) fn detect_frequency_from_millis(
    stamps: impl IntoIterator<Item = i64>,
) -> Option<Frequency> {
    let stamps: Vec<i64> = stamps.into_iter().collect();
    if stamps.len() < 2 {
        return None;
    }
    let mut gaps: Vec<i64> = stamps
        .windows(2)
        .map(|w| w[1] - w[0])
        .filter(|&g| g > 0)
        .collect();
    if gaps.is_empty() {
        return None;
    }
    gaps.sort_unstable();
    let median_ms = gaps[gaps.len() / 2];
    Some(snap_seconds_to_frequency(median_ms / 1000))
}

/// Parse one time-column value into a UTC-millisecond epoch stamp, or `None`
/// if no supported shape matches. Ordering the parse attempts by falling
/// specificity keeps the ambiguous cases sensible: an integer is treated as
/// an epoch first (so `1_704_067_200` doesn't try to parse as a date).
///
/// The `--series` loader (`crate::data`) calls this to populate `Atom::time`
/// at load, so the calendar indicators (`!year`, `!month`, …) and the
/// duration-form `-w/--windowed` resolver both work on the real timeline
/// without re-parsing.
pub(crate) fn parse_time_to_millis(raw: &str) -> Option<i64> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(n) = s.parse::<i64>() {
        // A stamp much larger than "seconds since epoch could plausibly be" is
        // almost certainly milliseconds — 10^11 seconds is ~year 5138.
        return Some(if n.abs() > 100_000_000_000 { n } else { n * 1000 });
    }
    if let Ok(dt) = time::OffsetDateTime::parse(s, &Rfc3339) {
        return Some(dt.unix_timestamp() * 1000);
    }
    let dt_fmt = format_description!("[year]-[month]-[day] [hour]:[minute]:[second]");
    if let Ok(dt) = time::PrimitiveDateTime::parse(s, dt_fmt) {
        return Some(dt.assume_utc().unix_timestamp() * 1000);
    }
    let date_fmt = format_description!("[year]-[month]-[day]");
    if let Ok(date) = time::Date::parse(s, date_fmt) {
        return Some(date.midnight().assume_utc().unix_timestamp() * 1000);
    }
    None
}

/// A `SYMBOL[FREQ]:` scope prefix. Either half is optional; both empty is the
/// unscoped "default" entry. Same grammar as the `--costs` and `--overlay`
/// prefixes (see [`crate::costs`], [`crate::overlay`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Scope {
    pub symbol: Option<String>,
    pub freq: Option<Frequency>,
}

impl Scope {
    /// True when both halves match the run's (symbol, effective freq). An
    /// absent half matches anything; a present symbol matches by equality; a
    /// present freq matches only when the run's freq is `Some(_)` and equal.
    pub fn matches(&self, symbol: &str, freq: Option<Frequency>) -> bool {
        let sym_ok = self.symbol.as_deref().is_none_or(|s| s == symbol);
        let freq_ok = self.freq.is_none_or(|f| Some(f) == freq);
        sym_ok && freq_ok
    }

    /// The default (unscoped) entry — neither half set.
    pub fn is_default(&self) -> bool {
        self.symbol.is_none() && self.freq.is_none()
    }

    /// Scope specificity for picking the winning entry: full `SYM[FREQ]` > `SYM`
    /// > `[FREQ]` > default. Higher number wins.
    fn specificity(&self) -> u8 {
        match (self.symbol.is_some(), self.freq.is_some()) {
            (true, true) => 3,
            (true, false) => 2,
            (false, true) => 1,
            (false, false) => 0,
        }
    }
}

/// Split off a leading `SYMBOL[FREQ]:` prefix from `text` at bracket depth
/// zero. Returns the (possibly default) scope and the remainder; the caller
/// parses the remainder into the value.
fn split_scope(text: &str) -> Result<(Scope, &str), String> {
    let mut depth: i32 = 0;
    for (i, ch) in text.char_indices() {
        match ch {
            '[' | '{' => depth += 1,
            ']' | '}' => depth -= 1,
            ':' if depth == 0 => {
                let (head, tail) = (text[..i].trim(), &text[i + 1..]);
                return Ok((parse_scope(head)?, tail));
            }
            _ => {}
        }
    }
    Ok((Scope::default(), text))
}

/// Split a `SYMBOL[FREQ]` token into its two raw parts (symbol as owned string,
/// freq as a borrowed slice for the caller to parse into whatever concrete type
/// it uses — [`Frequency`] on this side, [`Interval`] on the overlay side).
/// Rejects `SYMBOL[]`, an unclosed bracket, and the empty-both case.
///
/// The raw shared bracket grammar behind [`parse_scope`] and
/// [`crate::overlay::OverlayScope`]'s parser.
pub(crate) fn parse_scope_parts(text: &str) -> Result<(Option<String>, Option<&str>), String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok((None, None));
    }
    let (sym_part, freq_part) = match text.find('[') {
        Some(open) => {
            if !text.ends_with(']') {
                return Err(format!("scope `{text}`: `[freq]` bracket must close at the end"));
            }
            (text[..open].trim(), Some(text[open + 1..text.len() - 1].trim()))
        }
        None => (text, None),
    };
    let symbol = (!sym_part.is_empty()).then(|| sym_part.to_string());
    let freq = match freq_part {
        Some("") => return Err(format!("scope `{text}`: empty `[freq]` bracket")),
        Some(f) => Some(f),
        None => None,
    };
    if symbol.is_none() && freq.is_none() {
        return Err(format!("scope `{text}`: neither symbol nor freq present"));
    }
    Ok((symbol, freq))
}

/// Parse a bare `SYMBOL[FREQ]` prefix (no trailing colon), or return the
/// default scope for an empty string. At least one half must be present.
///
/// Shared by [`crate::costs`] (`--costs` argument parsing) and this module's
/// `--bars-per-year` / `-f/--frequency` prefixes.
pub(crate) fn parse_scope(text: &str) -> Result<Scope, String> {
    let (symbol, freq_str) = parse_scope_parts(text)?;
    let freq = match freq_str {
        Some(f) => Some(Frequency::from_str(f).map_err(|e| format!("scope `{text}`: {e}"))?),
        None => None,
    };
    Ok(Scope { symbol, freq })
}

/// One `--bars-per-year` argument, parsed as either a plain `N` (the
/// unscoped default entry) or a `SYMBOL[FREQ]:N` override that only applies
/// when the strategy's (symbol, effective freq) matches. See
/// [`pick_bars_per_year`] for the resolution rules.
#[derive(Debug, Clone, PartialEq)]
pub struct BarsPerYearSpec {
    pub scope: Scope,
    pub value: Real,
}

impl FromStr for BarsPerYearSpec {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (scope, body) = split_scope(s.trim())?;
        let body = body.trim();
        if body.is_empty() {
            return Err(format!("`{s}`: missing bars-per-year value after scope"));
        }
        let value: Real = body
            .parse()
            .map_err(|_| format!("`{s}`: `{body}` is not a number"))?;
        if !value.is_finite() || value <= 0.0 {
            return Err(format!(
                "`{s}`: bars-per-year must be a finite positive number (got {value})"
            ));
        }
        Ok(BarsPerYearSpec { scope, value })
    }
}

/// Pick the winning `bars_per_year` for a run from the (repeatable)
/// `--bars-per-year` entries and the resolved `(symbol, effective_freq)`.
/// Highest scope specificity wins (`SYM[FREQ]` > `SYM` > `[FREQ]` > default);
/// ties break to the last-declared entry so later flags override earlier
/// ones. Returns `None` when no entry matches — the caller then falls back
/// to the class × frequency calendar (see [`resolve`]).
pub fn pick_bars_per_year(
    specs: &[BarsPerYearSpec],
    symbol: &str,
    freq: Option<Frequency>,
) -> Option<Real> {
    specs
        .iter()
        .filter(|s| s.scope.matches(symbol, freq))
        .max_by_key(|s| s.scope.specificity())
        .map(|s| s.value)
}

/// One `--frequency` argument, parsed as either a plain `CODE` (the
/// unscoped default entry) or a `SYMBOL:CODE` override that only applies
/// when the strategy's symbol matches. The `[FREQ]:` half of the general
/// scope grammar is rejected here — pinning a cadence override to a
/// specific cadence would be circular. See [`pick_frequency`] for the
/// resolution rules.
#[derive(Debug, Clone, PartialEq)]
pub struct ScopedFrequency {
    pub symbol: Option<String>,
    pub value: Frequency,
}

impl FromStr for ScopedFrequency {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (scope, body) = split_scope(s.trim())?;
        if scope.freq.is_some() {
            return Err(format!(
                "`{s}`: `[freq]` scope isn't meaningful on --frequency (the value is a freq)"
            ));
        }
        let body = body.trim();
        if body.is_empty() {
            return Err(format!("`{s}`: missing frequency code after scope"));
        }
        let value =
            Frequency::from_str(body).map_err(|e| format!("`{s}`: {e}"))?;
        Ok(ScopedFrequency {
            symbol: scope.symbol,
            value,
        })
    }
}

/// Pick the effective bar cadence for `symbol` from the (repeatable)
/// `--frequency` entries. A symbol-scoped `SYM:CODE` wins over the
/// unscoped default; ties break to the last-declared entry so later flags
/// override earlier ones. Returns `None` when no entry matches — the
/// caller then falls back to auto-detection (see [`detect_frequency`]).
pub fn pick_frequency(specs: &[ScopedFrequency], symbol: &str) -> Option<Frequency> {
    specs
        .iter()
        .filter(|s| s.symbol.as_deref().is_none_or(|sym| sym == symbol))
        .max_by_key(|s| u8::from(s.symbol.is_some()))
        .map(|s| s.value)
}

/// Snap a per-bar delta in seconds to the closest named [`Frequency`]. The
/// boundaries are the (rounded) geometric means between adjacent cadences,
/// so a value equidistant in log space picks the smaller cadence.
fn snap_seconds_to_frequency(secs: i64) -> Frequency {
    match secs.max(1) {
        s if s < 134 => Frequency::Minute(1), // sqrt(60·300)
        s if s < 520 => Frequency::Minute(5), // sqrt(300·900)
        s if s < 1_273 => Frequency::Minute(15), // sqrt(900·1800)
        s if s < 2_545 => Frequency::Minute(30), // sqrt(1800·3600)
        s if s < 7_200 => Frequency::Hour(1), // sqrt(3600·14400)
        s if s < 35_300 => Frequency::Hour(4), // sqrt(14400·86400)
        s if s < 228_700 => Frequency::Day(1), // sqrt(86400·604800)
        s if s < 1_252_000 => Frequency::Week(1), // sqrt(604800·2592000)
        _ => Frequency::Month(1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frequency_parses_common_codes() {
        assert_eq!(Frequency::from_str("1m").unwrap(), Frequency::Minute(1));
        assert_eq!(Frequency::from_str("15m").unwrap(), Frequency::Minute(15));
        assert_eq!(Frequency::from_str("4h").unwrap(), Frequency::Hour(4));
        assert_eq!(Frequency::from_str("1d").unwrap(), Frequency::Day(1));
        assert_eq!(Frequency::from_str("1w").unwrap(), Frequency::Week(1));
        assert_eq!(Frequency::from_str("1M").unwrap(), Frequency::Month(1));
    }

    #[test]
    fn frequency_rejects_bad_input() {
        assert!(Frequency::from_str("").is_err());
        assert!(Frequency::from_str("m").is_err()); // missing multiplier
        assert!(Frequency::from_str("1x").is_err()); // unknown unit
        assert!(Frequency::from_str("0d").is_err()); // zero multiplier
        assert!(Frequency::from_str("abc").is_err());
    }

    #[test]
    fn parse_interval_parses_all_units() {
        assert_eq!(parse_interval("5m").unwrap(), Interval::Minute(5));
        assert_eq!(parse_interval("4h").unwrap(), Interval::Hour(4));
        assert_eq!(parse_interval("1d").unwrap(), Interval::Day(1));
        assert_eq!(parse_interval("1w").unwrap(), Interval::Week(1));
        assert_eq!(parse_interval("1M").unwrap(), Interval::Month(1));
    }

    #[test]
    fn parse_interval_rejects_zero_multiplier() {
        assert!(parse_interval("0d").is_err());
    }

    #[test]
    fn bars_per_year_matches_conventions() {
        // Daily bars — the canonical numbers.
        assert_eq!(AssetClass::Stocks.bars_per_year(Frequency::Day(1)), 252.0);
        assert_eq!(AssetClass::Forex.bars_per_year(Frequency::Day(1)), 260.0);
        assert_eq!(AssetClass::Crypto.bars_per_year(Frequency::Day(1)), 365.0);

        // Hourly bars — hours per trading year.
        assert_eq!(AssetClass::Stocks.bars_per_year(Frequency::Hour(1)), 252.0 * 6.5);
        assert_eq!(AssetClass::Crypto.bars_per_year(Frequency::Hour(1)), 365.0 * 24.0);

        // Weekly/monthly are calendar-based across all classes.
        assert_eq!(AssetClass::Stocks.bars_per_year(Frequency::Week(1)), 52.0);
        assert_eq!(AssetClass::Crypto.bars_per_year(Frequency::Month(1)), 12.0);
    }

    #[test]
    fn resolve_priority_explicit_wins() {
        // Explicit override beats derivation.
        assert_eq!(
            resolve(Some(999.0), Some(AssetClass::Crypto), Some(Frequency::Day(1))),
            999.0
        );
    }

    #[test]
    fn resolve_class_plus_frequency_derives() {
        assert_eq!(
            resolve(None, Some(AssetClass::Crypto), Some(Frequency::Day(1))),
            365.0
        );
        assert_eq!(
            resolve(None, Some(AssetClass::Stocks), Some(Frequency::Hour(1))),
            252.0 * 6.5
        );
    }

    #[test]
    fn resolve_falls_back_to_legacy_default() {
        // Nothing set → equities daily = 252 (backward-compatible default).
        assert_eq!(resolve(None, None, None), 252.0);
    }

    /// Snap millisecond stamps from parsed time strings — mirrors what
    /// [`crate::data::DataFrame::atoms`] does at load: parse each label into
    /// milliseconds and let the shared snap-to-cadence core do the work.
    fn detect_from_strs<'a>(times: impl IntoIterator<Item = &'a str>) -> Option<Frequency> {
        detect_frequency_from_millis(times.into_iter().filter_map(parse_time_to_millis))
    }

    #[test]
    fn detect_frequency_from_iso_dates() {
        let times = ["2024-01-01", "2024-01-02", "2024-01-03", "2024-01-04"];
        assert_eq!(detect_from_strs(times), Some(Frequency::Day(1)));
    }

    #[test]
    fn detect_frequency_from_rfc3339() {
        let times = [
            "2024-01-01T00:00:00Z",
            "2024-01-01T01:00:00Z",
            "2024-01-01T02:00:00Z",
        ];
        assert_eq!(detect_from_strs(times), Some(Frequency::Hour(1)));
    }

    #[test]
    fn detect_frequency_from_epoch_seconds() {
        // 5-minute cadence in Unix seconds.
        let times = ["1_704_067_200", "1_704_067_500", "1_704_067_800"]
            .map(|s| s.replace('_', ""));
        assert_eq!(
            detect_from_strs(times.iter().map(String::as_str)),
            Some(Frequency::Minute(5))
        );
    }

    #[test]
    fn detect_frequency_from_epoch_millis() {
        // 4-hour cadence in Unix millis — same instants times 1000.
        let times = ["1_704_067_200_000", "1_704_081_600_000", "1_704_096_000_000"]
            .map(|s| s.replace('_', ""));
        assert_eq!(
            detect_from_strs(times.iter().map(String::as_str)),
            Some(Frequency::Hour(4))
        );
    }

    #[test]
    fn detect_frequency_uses_median_and_ignores_gaps() {
        // A weekend gap between Fri and Mon is 3 days, but four Mon-Fri gaps
        // of 1 day dominate — the median holds and the result is still daily.
        let times = [
            "2024-01-01", "2024-01-02", "2024-01-03", "2024-01-04", "2024-01-05",
            "2024-01-08", "2024-01-09", "2024-01-10", "2024-01-11", "2024-01-12",
        ];
        assert_eq!(detect_from_strs(times), Some(Frequency::Day(1)));
    }

    #[test]
    fn detect_frequency_snaps_to_nearest_cadence() {
        // A 3-minute gap doesn't map to any named cadence — it snaps to `5m`
        // (its nearer neighbour in log space).
        let times = [
            "2024-01-01T00:00:00Z",
            "2024-01-01T00:03:00Z",
            "2024-01-01T00:06:00Z",
            "2024-01-01T00:09:00Z",
        ];
        assert_eq!(detect_from_strs(times), Some(Frequency::Minute(5)));
    }

    #[test]
    fn detect_frequency_gives_up_on_unparseable() {
        // Opaque, non-time strings — no median → no detection.
        let times = ["alpha", "beta", "gamma"];
        assert!(detect_from_strs(times).is_none());
    }

    #[test]
    fn detect_frequency_needs_two_parseable_stamps() {
        // One time isn't enough to compute a gap.
        assert!(detect_from_strs(["2024-01-01"]).is_none());
    }

    #[test]
    fn detect_frequency_from_atoms_reads_atom_time() {
        // Same as detect_frequency_from_rfc3339 but reading Atom.time directly
        // — the real code path exercised by run.rs / optimize.rs.
        let candle = Candle::new(1.0, 1.0, 1.0, 1.0, 0.0);
        let atoms = [
            Atom::with_time(candle, Timestamp(0)),
            Atom::with_time(candle, Timestamp(3_600_000)),
            Atom::with_time(candle, Timestamp(7_200_000)),
        ];
        assert_eq!(
            detect_frequency_from_atoms(atoms.iter()),
            Some(Frequency::Hour(1))
        );
    }

    fn spec(s: &str) -> BarsPerYearSpec {
        s.parse().unwrap()
    }

    #[test]
    fn bars_per_year_spec_parses_plain_number() {
        let s = spec("252");
        assert_eq!(s.scope, Scope::default());
        assert_eq!(s.value, 252.0);
    }

    #[test]
    fn bars_per_year_spec_parses_scoped_forms() {
        assert_eq!(spec("BTC:8760").scope.symbol.as_deref(), Some("BTC"));
        assert_eq!(spec("BTC:8760").scope.freq, None);
        assert_eq!(spec("[1h]:8760").scope.symbol, None);
        assert_eq!(spec("[1h]:8760").scope.freq, Some(Frequency::Hour(1)));
        let s = spec("BTC[1h]:8760");
        assert_eq!(s.scope.symbol.as_deref(), Some("BTC"));
        assert_eq!(s.scope.freq, Some(Frequency::Hour(1)));
        assert_eq!(s.value, 8760.0);
    }

    #[test]
    fn bars_per_year_spec_rejects_bad_input() {
        // Value missing entirely.
        assert!("BTC:".parse::<BarsPerYearSpec>().is_err());
        // Value not a number.
        assert!("BTC:oops".parse::<BarsPerYearSpec>().is_err());
        // Non-positive value.
        assert!("0".parse::<BarsPerYearSpec>().is_err());
        assert!("-1".parse::<BarsPerYearSpec>().is_err());
        // Empty freq bracket.
        assert!("BTC[]:8760".parse::<BarsPerYearSpec>().is_err());
        // Bracket doesn't close at end.
        assert!("BTC[1h:8760".parse::<BarsPerYearSpec>().is_err());
    }

    #[test]
    fn pick_bars_per_year_prefers_specificity() {
        // Full > symbol > freq > default; last-declared wins on a tie.
        let specs: Vec<BarsPerYearSpec> = [
            "500", // default
            "BTC:1000",
            "[1h]:2000",
            "BTC[1h]:4000",
            "ETH[1d]:9999",
        ]
        .iter()
        .map(|s| spec(s))
        .collect();
        assert_eq!(
            pick_bars_per_year(&specs, "BTC", Some(Frequency::Hour(1))),
            Some(4000.0)
        );
        assert_eq!(
            pick_bars_per_year(&specs, "BTC", Some(Frequency::Day(1))),
            Some(1000.0)
        );
        assert_eq!(
            pick_bars_per_year(&specs, "SOL", Some(Frequency::Hour(1))),
            Some(2000.0)
        );
        assert_eq!(pick_bars_per_year(&specs, "SOL", None), Some(500.0));
    }

    #[test]
    fn pick_bars_per_year_falls_through_when_nothing_matches() {
        // Only a specific scope declared; the run's (symbol, freq) doesn't match.
        let specs = vec![spec("BTC[1h]:8760")];
        assert_eq!(
            pick_bars_per_year(&specs, "AAPL", Some(Frequency::Day(1))),
            None
        );
    }

    #[test]
    fn pick_bars_per_year_last_declared_wins_at_tie() {
        // Two equally specific entries — later one wins.
        let specs = vec![spec("BTC:100"), spec("BTC:200")];
        assert_eq!(pick_bars_per_year(&specs, "BTC", None), Some(200.0));
    }

    fn fspec(s: &str) -> ScopedFrequency {
        s.parse().unwrap()
    }

    #[test]
    fn frequency_spec_parses_plain_and_symbol_scoped() {
        assert_eq!(fspec("1d").symbol, None);
        assert_eq!(fspec("1d").value, Frequency::Day(1));
        assert_eq!(fspec("BTC:4h").symbol.as_deref(), Some("BTC"));
        assert_eq!(fspec("BTC:4h").value, Frequency::Hour(4));
    }

    #[test]
    fn frequency_spec_rejects_freq_scope() {
        // `[FREQ]:` scope on a --frequency value is circular — rejected.
        assert!("[1h]:4h".parse::<ScopedFrequency>().is_err());
        assert!("BTC[1h]:4h".parse::<ScopedFrequency>().is_err());
    }

    #[test]
    fn frequency_spec_rejects_bad_input() {
        assert!("BTC:".parse::<ScopedFrequency>().is_err());
        assert!("BTC:oops".parse::<ScopedFrequency>().is_err());
    }

    #[test]
    fn pick_frequency_prefers_symbol_over_default() {
        let specs = vec![fspec("1d"), fspec("BTC:4h"), fspec("ETH:1h")];
        assert_eq!(pick_frequency(&specs, "BTC"), Some(Frequency::Hour(4)));
        assert_eq!(pick_frequency(&specs, "ETH"), Some(Frequency::Hour(1)));
        assert_eq!(pick_frequency(&specs, "SOL"), Some(Frequency::Day(1)));
    }

    #[test]
    fn pick_frequency_returns_none_when_nothing_matches() {
        let specs = vec![fspec("BTC:4h")];
        assert_eq!(pick_frequency(&specs, "AAPL"), None);
    }

    #[test]
    fn pick_frequency_last_declared_wins_at_tie() {
        let specs = vec![fspec("BTC:1d"), fspec("BTC:4h")];
        assert_eq!(pick_frequency(&specs, "BTC"), Some(Frequency::Hour(4)));
    }

    #[test]
    fn frequency_orders_by_duration() {
        // Canonical cadences in ascending duration.
        assert!(Frequency::Minute(1) < Frequency::Minute(5));
        assert!(Frequency::Minute(30) < Frequency::Hour(1));
        assert!(Frequency::Hour(1) < Frequency::Hour(4));
        assert!(Frequency::Hour(4) < Frequency::Day(1));
        assert!(Frequency::Day(1) < Frequency::Week(1));
        assert!(Frequency::Week(1) < Frequency::Month(1));
    }

    #[test]
    fn frequency_ord_handles_exotic_multipliers() {
        // Exotic user input: 120m parses as Minute(120) and must rank *after*
        // Hour(1), which derived Ord would get wrong (lexicographic).
        assert!(Frequency::Minute(120) > Frequency::Hour(1));
        // Same-duration pairs still stay distinct via derived `PartialEq`
        // (Ord ties break by variant, so `Hour(24)` sorts before `Day(1)`).
        assert!(Frequency::Hour(24) < Frequency::Day(1));
        assert_ne!(Frequency::Hour(24), Frequency::Day(1));
    }

    #[test]
    fn window_spec_parses_bar_count_and_durations() {
        assert_eq!(
            "10".parse::<WindowSpec>().unwrap(),
            WindowSpec::Bars(NonZeroUsize::new(10).unwrap())
        );
        assert_eq!(
            "252".parse::<WindowSpec>().unwrap(),
            WindowSpec::Bars(NonZeroUsize::new(252).unwrap())
        );
        assert_eq!(
            "1d".parse::<WindowSpec>().unwrap(),
            WindowSpec::Duration(Frequency::Day(1))
        );
        assert_eq!(
            "4h".parse::<WindowSpec>().unwrap(),
            WindowSpec::Duration(Frequency::Hour(4))
        );
        assert_eq!(
            "1M".parse::<WindowSpec>().unwrap(),
            WindowSpec::Duration(Frequency::Month(1))
        );
    }

    #[test]
    fn window_spec_rejects_bad_input() {
        assert!("".parse::<WindowSpec>().is_err()); // empty
        assert!("0".parse::<WindowSpec>().is_err()); // zero bars
        assert!("0d".parse::<WindowSpec>().is_err()); // zero duration
        assert!("1x".parse::<WindowSpec>().is_err()); // unknown unit
        assert!("abc".parse::<WindowSpec>().is_err());
    }

    #[test]
    fn window_spec_bars_form_is_identity() {
        // Plain bar count needs neither cadence nor class.
        let n = WindowSpec::Bars(NonZeroUsize::new(42).unwrap())
            .resolve(None, None)
            .unwrap();
        assert_eq!(n.get(), 42);
    }

    #[test]
    fn window_spec_duration_stocks_daily_windows() {
        // `-w 1w` on daily stocks → 5 M-F bars.
        let n = WindowSpec::Duration(Frequency::Week(1))
            .resolve(Some(Frequency::Day(1)), Some(AssetClass::Stocks))
            .unwrap();
        assert_eq!(n.get(), 5);
        // `-w 1M` on daily stocks → 21 trading days (252/12).
        let n = WindowSpec::Duration(Frequency::Month(1))
            .resolve(Some(Frequency::Day(1)), Some(AssetClass::Stocks))
            .unwrap();
        assert_eq!(n.get(), 21);
    }

    #[test]
    fn window_spec_duration_stocks_intraday_windows() {
        // `-w 1d` on hourly stocks is one 6.5-hour trading day, not 24 —
        // rounds to 7 hourly bars.
        let n = WindowSpec::Duration(Frequency::Day(1))
            .resolve(Some(Frequency::Hour(1)), Some(AssetClass::Stocks))
            .unwrap();
        assert_eq!(n.get(), 7);
        // `-w 4h` on hourly bars is 4 bars regardless of class.
        let n = WindowSpec::Duration(Frequency::Hour(4))
            .resolve(Some(Frequency::Hour(1)), Some(AssetClass::Stocks))
            .unwrap();
        assert_eq!(n.get(), 4);
        // `-w 1w` on hourly stocks: 252 trading days ÷ 52 weeks × 6.5 h ≈
        // 31.5 hourly bars → 32. The 252/52 formulation is annualization-
        // consistent (52 weeks × ⁿ = 12 months × ⁿ = 252 days) — a caller
        // who prefers the calendar-week convention of exactly 5×6.5 = 32.5
        // hourly bars per week can pass `-w 33`.
        let n = WindowSpec::Duration(Frequency::Week(1))
            .resolve(Some(Frequency::Hour(1)), Some(AssetClass::Stocks))
            .unwrap();
        assert_eq!(n.get(), 32);
    }

    #[test]
    fn window_spec_duration_crypto_is_24_7() {
        // Continuous crypto: `-w 1d` = 24 hourly bars, `-w 1w` on daily = 7.
        let n = WindowSpec::Duration(Frequency::Day(1))
            .resolve(Some(Frequency::Hour(1)), Some(AssetClass::Crypto))
            .unwrap();
        assert_eq!(n.get(), 24);
        let n = WindowSpec::Duration(Frequency::Week(1))
            .resolve(Some(Frequency::Day(1)), Some(AssetClass::Crypto))
            .unwrap();
        assert_eq!(n.get(), 7);
    }

    #[test]
    fn window_spec_duration_errors_without_class() {
        // Missing class — silent Stocks would mis-interpret intraday
        // durations on 24h markets; the CLI must surface the omission.
        let err = WindowSpec::Duration(Frequency::Day(1))
            .resolve(Some(Frequency::Hour(1)), None)
            .unwrap_err();
        assert!(
            err.contains("trading calendar"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn window_spec_duration_errors_without_frequency() {
        // Class is fine, cadence is missing — the caller must pass
        // `-f/--frequency` or provide input with parseable timestamps.
        let err = WindowSpec::Duration(Frequency::Day(1))
            .resolve(None, Some(AssetClass::Stocks))
            .unwrap_err();
        assert!(err.contains("no bar cadence"), "unexpected error: {err}");
    }

    #[test]
    fn window_spec_duration_errors_when_shorter_than_bar() {
        // `-w 1d` on 1w bars would round to 0 bars — rejected.
        let err = WindowSpec::Duration(Frequency::Day(1))
            .resolve(Some(Frequency::Week(1)), Some(AssetClass::Stocks))
            .unwrap_err();
        assert!(err.contains("shorter than one bar"), "unexpected error: {err}");
    }

    #[test]
    fn window_spec_display_roundtrips() {
        // The Display impl mirrors what the parser accepts.
        assert_eq!(
            format!("{}", "10".parse::<WindowSpec>().unwrap()),
            "10"
        );
        assert_eq!(
            format!("{}", "1w".parse::<WindowSpec>().unwrap()),
            "1w"
        );
        assert_eq!(
            format!("{}", "1M".parse::<WindowSpec>().unwrap()),
            "1M"
        );
    }

    #[test]
    fn frequency_sort_produces_semantic_order() {
        // Sortable in a Vec — verifies the Ord + PartialOrd impls compose.
        let mut fs = vec![
            Frequency::Day(1),
            Frequency::Minute(5),
            Frequency::Hour(1),
            Frequency::Week(1),
        ];
        fs.sort();
        assert_eq!(
            fs,
            vec![
                Frequency::Minute(5),
                Frequency::Hour(1),
                Frequency::Day(1),
                Frequency::Week(1),
            ]
        );
    }
}
