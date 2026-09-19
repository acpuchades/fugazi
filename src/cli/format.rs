//! Pure console formatting — `&Metrics`-and-numbers in, `String` out.
//!
//! The `print_*` blocks in `run.rs` / `optimize.rs` are IO and stay with
//! their subcommand; everything here is a pure function they call, moved out
//! so the *numeric truth* of a console line is unit-testable without driving
//! the binary through `Command` (docs/TESTING.md's "a failure names the
//! function rather than the subcommand"). The `—` em-dash is the shared
//! spelling of "not measurable on this run" across every formatter.

use fugazi::prelude::*;

use crate::metrics;
use crate::optimize::{Evaluation, lookup, lookup_windowed};

/// The metrics block's holding-time line: `avg N bars (~dur) · min … · max …`,
/// collapsed to a single value when every trade held the same number of bars.
/// `None` when the run closed no trade.
pub(crate) fn format_holding_line(
    m: &metrics::Metrics,
    bar_freq: Option<Frequency>,
) -> Option<String> {
    let avg = m.trades.average_bars;
    let min = m.trades.min_bars.map(|n| n as Real);
    let max = m.trades.max_bars.map(|n| n as Real);
    if avg.is_none() && min.is_none() && max.is_none() {
        return None;
    }
    let bars_str = |bars: Real, precision: usize| -> String {
        let dur = bar_freq
            .map(|f| format!(" (~{})", format_bars_as_duration(bars, f)))
            .unwrap_or_default();
        format!("{bars:.*} bars{dur}", precision)
    };
    // Collapse to a single value when the three legs coincide (either one
    // trade, or every trade held the exact same number of bars). Uses a
    // 1e-6 tolerance since `avg` is a Real from a running mean.
    if let (Some(avg), Some(min), Some(max)) = (avg, min, max)
        && (avg - min).abs() < 1e-6
        && (avg - max).abs() < 1e-6
    {
        let precision = if avg.fract().abs() < 1e-6 { 0 } else { 1 };
        return Some(bars_str(avg, precision));
    }
    let leg = |label: &str, bars: Option<Real>, precision: usize| -> Option<String> {
        Some(format!("{label} {}", bars_str(bars?, precision)))
    };
    let parts: Vec<String> = [leg("avg", avg, 1), leg("min", min, 0), leg("max", max, 0)]
        .into_iter()
        .flatten()
        .collect();
    Some(parts.join(" · "))
}

/// Render `bars` bars of `freq` cadence as a duration in the cadence's own
/// unit alphabet (`21d`, `4h`, `26h` — `Frequency::from_str`-compatible for
/// integer counts). Fractional averages carry one decimal.
pub(crate) fn format_bars_as_duration(bars: Real, freq: Frequency) -> String {
    let (mult, letter) = match freq {
        Frequency::Minute(n) => (n, "m"),
        Frequency::Hour(n) => (n, "h"),
        Frequency::Day(n) => (n, "d"),
        Frequency::Week(n) => (n, "w"),
        Frequency::Month(n) => (n, "M"),
    };
    let total = bars * mult as Real;
    if (total - total.round()).abs() < 1e-6 {
        format!("{total:.0}{letter}")
    } else {
        format!("{total:.1}{letter}")
    }
}

pub(crate) fn format_ratio(v: Option<Real>) -> String {
    v.map_or_else(|| "—".to_string(), |r| format!("{r:.2}"))
}

pub(crate) fn format_pct(v: Option<Real>) -> String {
    v.map_or_else(|| "—".to_string(), |r| format!("{r:.1}%"))
}

/// One headline stat's cross-window `mean ± std` inputs, aggregated over the
/// non-overlapping `-w` windows. Windows where the stat is degenerate (no
/// losing trade for a profit factor, zero variance for Sharpe, …) are dropped
/// via the `Option` filter — a stat with fewer than one defined window
/// renders as `—` downstream.
pub(crate) fn mean_std_of<F>(windows: &[metrics::WindowMetrics], f: F) -> Option<(Real, Real)>
where
    F: Fn(&metrics::Metrics) -> Option<Real>,
{
    metrics::mean_std(windows.iter().filter_map(|w| f(&w.metrics)))
}

/// `+M.MM ± S.SS%` — signed mean (returns can be negative), unsigned stddev,
/// unit suffix once at the end.
pub(crate) fn format_ms_signed_pct(pair: Option<(Real, Real)>) -> String {
    pair.map_or_else(|| "—".to_string(), |(m, s)| format!("{m:+.2} ± {s:.2}%"))
}

/// `M.MM ± S.SS%` — unsigned mean (magnitudes, ratios in percent form).
pub(crate) fn format_ms_unsigned_pct(pair: Option<(Real, Real)>) -> String {
    pair.map_or_else(|| "—".to_string(), |(m, s)| format!("{m:.2} ± {s:.2}%"))
}

/// `M.MM ± S.SS` — unitless ratio (Sharpe, Sortino, Omega, profit factor).
pub(crate) fn format_ms_ratio(pair: Option<(Real, Real)>) -> String {
    pair.map_or_else(|| "—".to_string(), |(m, s)| format!("{m:.2} ± {s:.2}"))
}

/// `M ± S` at `precision` decimals — for counts (trades, drawdown duration
/// bars) treated as floats so a fractional mean survives the format.
pub(crate) fn format_ms_count(pair: Option<(Real, Real)>, precision: usize) -> String {
    pair.map_or_else(
        || "—".to_string(),
        |(m, s)| format!("{m:.*} ± {s:.*}", precision, precision),
    )
}

/// `first → last (N bars[, W warm-up])` over the evaluated slice of a run's
/// time labels — `None` when warm-up consumed the whole stream.
pub(crate) fn evaluated_period_line(labels: &[String], warmup: usize) -> Option<String> {
    let evaluated = labels.get(warmup.min(labels.len())..)?;
    let (s, e) = (evaluated.first()?, evaluated.last()?);
    let bars = evaluated.len();
    Some(match warmup {
        0 => format!("{s} → {e} ({bars} bars)"),
        w => format!("{s} → {e} ({bars} bars, {w} warm-up)"),
    })
}

/// Short friendly label for the console — strip the section prefix from a
/// canonical dotted metric path (`risk_adjusted.sharpe` → `sharpe`,
/// `returns.cagr_pct` → `cagr_pct`). CSV columns stay as the canonical
/// dotted path — this is just for display.
pub(crate) fn friendly_metric_label(dotted_or_short: &str) -> String {
    dotted_or_short
        .rsplit_once('.')
        .map(|(_, tail)| tail.to_string())
        .unwrap_or_else(|| dotted_or_short.to_string())
}

/// One metric cell of the sweep table, in the evaluation's own shape: a point
/// estimate whole-run, `mean ± std` windowed, `mean ± std (n/m)` pooled.
pub(crate) fn format_metric(eval: &Evaluation, path: &str) -> String {
    match eval {
        Evaluation::Whole(m) => {
            lookup(m, path).map_or_else(|| "—".to_string(), |v| format!("{v:.4}"))
        }
        Evaluation::Windowed(ws) => lookup_windowed(ws, path).map_or_else(
            || "—".to_string(),
            |(mean, std)| format!("{mean:.4} ± {std:.4}"),
        ),
        // `± std (n/m)` — the support is inline rather than in a separate
        // field because the number is only interpretable next to it.
        Evaluation::Panel(ms) => crate::spec::panel::pool_metric(ms, path).map_or_else(
            || "—".to_string(),
            |p| format!("{:.4} ± {:.4} ({}/{})", p.mean, p.std, p.defined, p.members),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bars_render_in_the_cadence_own_unit() {
        assert_eq!(format_bars_as_duration(21.0, Frequency::Day(1)), "21d");
        assert_eq!(format_bars_as_duration(6.5, Frequency::Hour(4)), "26h");
        assert_eq!(format_bars_as_duration(2.25, Frequency::Hour(2)), "4.5h");
        assert_eq!(format_bars_as_duration(3.0, Frequency::Month(1)), "3M");
    }

    #[test]
    fn missing_values_render_as_an_em_dash_everywhere() {
        assert_eq!(format_ratio(None), "—");
        assert_eq!(format_pct(None), "—");
        assert_eq!(format_ms_signed_pct(None), "—");
        assert_eq!(format_ms_unsigned_pct(None), "—");
        assert_eq!(format_ms_ratio(None), "—");
        assert_eq!(format_ms_count(None, 1), "—");
    }

    #[test]
    fn mean_std_pairs_carry_their_sign_convention() {
        assert_eq!(format_ms_signed_pct(Some((1.234, 0.5))), "+1.23 ± 0.50%");
        assert_eq!(format_ms_signed_pct(Some((-1.2, 0.5))), "-1.20 ± 0.50%");
        assert_eq!(format_ms_unsigned_pct(Some((1.234, 0.5))), "1.23 ± 0.50%");
        assert_eq!(format_ms_ratio(Some((2.0, 0.25))), "2.00 ± 0.25");
        assert_eq!(format_ms_count(Some((3.5, 1.25)), 1), "3.5 ± 1.2");
    }

    #[test]
    fn the_evaluated_period_line_names_warmup_only_when_there_is_one() {
        let labels: Vec<String> = (1..=5).map(|d| format!("2024-01-0{d}")).collect();
        assert_eq!(
            evaluated_period_line(&labels, 0).as_deref(),
            Some("2024-01-01 → 2024-01-05 (5 bars)"),
        );
        assert_eq!(
            evaluated_period_line(&labels, 2).as_deref(),
            Some("2024-01-03 → 2024-01-05 (3 bars, 2 warm-up)"),
        );
        // Warm-up swallowing the whole stream is "nothing to report", not a
        // panic or an empty range.
        assert_eq!(evaluated_period_line(&labels, 5), None);
    }

    #[test]
    fn metric_labels_drop_their_section_prefix() {
        assert_eq!(friendly_metric_label("risk_adjusted.sharpe"), "sharpe");
        assert_eq!(friendly_metric_label("sharpe"), "sharpe");
    }
}
