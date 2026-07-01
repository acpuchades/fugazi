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

use std::str::FromStr;

use fugazi::prelude::*;

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

/// A bar cadence as an integer multiplier and unit — `5m`, `4h`, `1d`, `1w`,
/// `1M`. `M` for month is uppercase to keep `m` unambiguously "minute".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Frequency {
    Minute(u32),
    Hour(u32),
    Day(u32),
    Week(u32),
    Month(u32),
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
}
