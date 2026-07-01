//! Render the run's equity curve to `equity.png` in the output directory.
//!
//! Styled after pyfolio/empyrical's cumulative-returns plot: a single muted
//! blue line on a light two-tier grid, a subtle dashed baseline at the
//! starting cash for reference, and no fills. The x-axis is bar index with a
//! handful of ticks labelled from the source data's time strings. It's a
//! visual counterpart to `metrics.yml`, nothing more.

use std::path::Path;

use anyhow::{Context, Result};
use fugazi::prelude::*;
use plotters::prelude::*;

/// Muted steel blue — pyfolio/seaborn's default first palette color.
const LINE_COLOR: RGBColor = RGBColor(76, 114, 176);
/// Faint gray for minor gridlines.
const GRID_MINOR: RGBColor = RGBColor(235, 235, 235);
/// Slightly darker gray for major gridlines.
const GRID_MAJOR: RGBColor = RGBColor(210, 210, 210);

/// Write `equity.png` to `path`. `equity_curve[i]` is the mark-to-market
/// equity at the close of the bar labelled `times[i]`. `starting_cash` draws
/// the reference baseline. Silently no-ops on an empty curve (no bars ran).
pub fn write_equity_curve(
    equity_curve: &[Real],
    times: &[String],
    starting_cash: Real,
    path: &Path,
) -> Result<()> {
    if equity_curve.is_empty() {
        return Ok(());
    }
    let n = equity_curve.len();

    // Y-range covers the curve and the starting-cash baseline, with a small
    // pad. Floor the pad so a flat run doesn't collapse the axis.
    let mut lo = starting_cash;
    let mut hi = starting_cash;
    for &e in equity_curve {
        lo = lo.min(e);
        hi = hi.max(e);
    }
    let pad = ((hi - lo).abs() * 0.05).max(1e-9);
    let (y_lo, y_hi) = (lo - pad, hi + pad);

    // Clamp x-range width to at least 1 so a single-bar run still renders.
    let x_hi = ((n as f64) - 1.0).max(1.0);

    let root = BitMapBackend::new(path, (1280, 720)).into_drawing_area();
    root.fill(&WHITE)
        .map_err(|e| anyhow::anyhow!("filling background: {e}"))?;

    let mut chart = ChartBuilder::on(&root)
        .margin(24)
        .x_label_area_size(44)
        .y_label_area_size(72)
        .caption("Cumulative Returns", ("sans-serif", 22))
        .build_cartesian_2d(0f64..x_hi, y_lo..y_hi)
        .map_err(|e| anyhow::anyhow!("building chart: {e}"))?;

    // Sampled tick positions (first, quarters, mid, last). Small `n` dedups
    // down to fewer real positions naturally.
    let tick_positions: Vec<usize> = {
        let raw = [0, n / 4, n / 2, (3 * n) / 4, n.saturating_sub(1)];
        let mut v: Vec<usize> = raw.into_iter().filter(|i| *i < n).collect();
        v.sort_unstable();
        v.dedup();
        v
    };

    chart
        .configure_mesh()
        .x_labels(tick_positions.len().max(2))
        .x_label_formatter(&|x| {
            let i = (x.round() as usize).min(n.saturating_sub(1));
            times.get(i).cloned().unwrap_or_default()
        })
        .y_desc("Portfolio value")
        .label_style(("sans-serif", 12))
        .axis_desc_style(("sans-serif", 14))
        .light_line_style(GRID_MINOR)
        .bold_line_style(GRID_MAJOR)
        .draw()
        .map_err(|e| anyhow::anyhow!("drawing mesh: {e}"))?;

    // Subtle dashed baseline at starting cash.
    chart
        .draw_series(DashedLineSeries::new(
            [(0.0, starting_cash), (x_hi, starting_cash)],
            10,
            6,
            ShapeStyle::from(BLACK.mix(0.25)).stroke_width(1),
        ))
        .map_err(|e| anyhow::anyhow!("drawing baseline: {e}"))?;

    // Equity line.
    chart
        .draw_series(LineSeries::new(
            equity_curve.iter().enumerate().map(|(k, &e)| (k as f64, e)),
            LINE_COLOR.stroke_width(2),
        ))
        .map_err(|e| anyhow::anyhow!("drawing equity line: {e}"))?;

    root.present()
        .with_context(|| format!("writing `{}`", path.display()))?;
    Ok(())
}
