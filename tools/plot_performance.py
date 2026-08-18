#!/usr/bin/env python3
"""Render the TA-Lib comparison as an SVG bar chart for the README.

    pixi run -e bench bench            # writes docs/assets/performance-samples.json
    python3 tools/plot_performance.py  # reads it, writes docs/assets/performance.svg

**The numbers are read from the samples file, never typed here.** An earlier
version held a hand-pasted `ROWS` table, which drifted: it still claimed 4.97
ns/sample for `sma` through the bindings long after the real figure had moved,
and comparing against it manufactured a phantom regression. The benchmark already
writes every raw sample; the chart reads the same file.

Why hand-rolled SVG rather than matplotlib: no dependency (this has to run from a
bare checkout), no fonts baked into paths, and full control over the two things
that actually matter for a README image —

  * **Dark mode.** GitHub serves the same file on a white and a near-black
    background, and an `<img>` cannot inherit the page's colours. So there is no
    background fill, and every label uses one mid-grey (`#8b949e`) that stays
    legible on both. Do not "fix" it to a darker grey; it will vanish for half
    the readers.
  * **Being diffable.** The output is text, so a number changing shows up in
    review as a number changing.

All four bars are normalised to **native TA-Lib C**, so every tier sits on one
scale and `1.0x` reads as "as fast as the C library". Note this is *not* the
like-for-like ratio for the Python bindings — a Python user should compare
against `talib`, which is the `py vs py` column of the README table. The chart
answers "where does everything sit?"; the table answers "what do I give up?".
"""

from __future__ import annotations

import json
import os

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SAMPLES = os.path.join(REPO, "docs", "assets", "performance-samples.json")
OUT = os.path.join(REPO, "docs", "assets", "performance.svg")

# Row order, and the order bars appear within a row. Each library sits next to
# its own counterpart, so the two comparisons that mean something read straight
# off adjacent bars — native against native, then Python against Python.
# Interleaving them by language instead makes the reader do the pairing.
#
# Colour is by *library*, not by tier: one hue for TA-Lib, one for fugazi, the
# native build darker than the Python binding in each. So "which library" is the
# hue and "native or bindings" is the shade, and the eye groups the pairs without
# consulting the legend.
SERIES = [
    ("talib_c", "TA-Lib (C)", "#9e6a03"),
    ("fugazi_rs", "fugazi (Rust)", "#1f6feb"),
    ("talib_py", "talib (Python)", "#e3b341"),
    ("fugazi_py", "fugazi (Python)", "#79c0ff"),
]
# Grouped, because the two halves answer different questions and the multi-output
# rows are not comparable to the scalar ones bar-for-bar: a multi-output `update`
# produces every line, so its TA-Lib counterpart is the one call that fills every
# output array (and, for `dmi`/`adx`, the two and three calls TA-Lib needs
# because it has no combined entry point). See docs/PERFORMANCE.md, Phase 10.
GROUPS = [
    ("single output", ["sma", "ema", "rsi", "atr", "stddev"]),
    ("multi-output — every line, one pass", ["macd", "bbands", "aroon", "dmi", "adx"]),
]
INDICATORS = [i for _, group in GROUPS for i in group]

W = 780
LEFT, RIGHT, TOP = 78, 40, 76
BAR_H, GAP = 11, 3
ROW_H = len(SERIES) * (BAR_H + GAP) + 15
HEAD_H = 22               # vertical space a group heading takes

INK = "#8b949e"          # legible on white and on #0d1117

# One bar clips at this scale — `bbands` through the bindings, at ~10.8x — and it
# reads as clipped (caret, true value in the label) rather than as exactly XMAX.
# Widening the axis to fit it would compress every other bar into illegibility to
# accommodate the single row the project has already decided to lose on purpose.
XMAX = 5.0
PLOT_W = W - LEFT - RIGHT


def x(ratio: float) -> float:
    return LEFT + min(ratio, XMAX) / XMAX * PLOT_W


def percentile(sorted_vals: list[float], p: float) -> float:
    """Linear-interpolated percentile, `p` in [0, 1]. Matches R type 7."""
    if not sorted_vals:
        return 0.0
    if len(sorted_vals) == 1:
        return sorted_vals[0]
    h = (len(sorted_vals) - 1) * p
    lo = int(h)
    hi = min(lo + 1, len(sorted_vals) - 1)
    return sorted_vals[lo] + (h - lo) * (sorted_vals[hi] - sorted_vals[lo])


def render_row(add, stat, name: str, y0: float) -> None:
    """Emit one indicator's label and its four bars, top-left at `y0`."""
    base = stat["talib_c"][name][0]
    add(f'<text x="{LEFT - 10}" y="{y0 + 2 * (BAR_H + GAP) + 2}" fill="{INK}" '
        f'text-anchor="end" font-family="ui-monospace,SFMono-Regular,'
        f'Consolas,monospace">{name}</text>')

    for j, (key, _, colour) in enumerate(SERIES):
        if name not in stat[key]:
            continue
        lo, p25 = stat[key][name]
        val = lo / base
        by = y0 + j * (BAR_H + GAP)
        w = max(x(val) - LEFT, 1.0)
        add(f'<rect x="{LEFT}" y="{by}" width="{w:.1f}" height="{BAR_H}" '
            f'rx="2" fill="{colour}"/>')

        # Upward whisker to the 25th percentile, drawn only when it is wide
        # enough to mean anything at this scale.
        wx = x(p25 / base)
        if wx - (LEFT + w) > 1.5:
            cy = by + BAR_H / 2
            add(f'<line x1="{LEFT + w:.1f}" y1="{cy:.1f}" x2="{wx:.1f}" y2="{cy:.1f}" '
                f'stroke="{INK}" stroke-width="1" opacity="0.8"/>')
            add(f'<line x1="{wx:.1f}" y1="{by + 2:.1f}" x2="{wx:.1f}" '
                f'y2="{by + BAR_H - 2:.1f}" stroke="{INK}" stroke-width="1" '
                f'opacity="0.8"/>')

        clipped = val > XMAX
        text = f'{val:.2f}x{" &#8250;" if clipped else ""}'
        # Inside the bar when it is wide enough, outside when it is not.
        if w > 52:
            add(f'<text x="{LEFT + w - 5:.1f}" y="{by + BAR_H - 1}" '
                f'fill="#ffffff" text-anchor="end" font-size="11">{text}</text>')
        else:
            add(f'<text x="{max(wx, LEFT + w) + 5:.1f}" y="{by + BAR_H - 1}" '
                f'fill="{INK}" font-size="11">{text}</text>')


def main() -> int:
    if not os.path.exists(SAMPLES):
        raise SystemExit(
            f"{os.path.relpath(SAMPLES, REPO)} not found — run `pixi run -e bench bench` first"
        )
    with open(SAMPLES) as f:
        blob = json.load(f)
    data = blob["samples"]
    n = blob["n"]

    # The benchmark reports the *minimum*: contention only ever adds time, so the
    # fastest observation is the closest to the machine's actual capability.
    # The whisker therefore runs one way only — from that minimum up to the 25th
    # percentile, i.e. "the bar is the best case, a quarter of runs land by here".
    # A symmetric error bar would imply the mean is the estimate, which it is not.
    stat: dict[str, dict[str, tuple[float, float]]] = {}
    for key, _, _ in SERIES:
        stat[key] = {}
        for ind in INDICATORS:
            vals = sorted(data.get(key, {}).get(ind, []))
            if vals:
                stat[key][ind] = (vals[0], percentile(vals, 0.25))

    groups = [(title, [i for i in g if i in stat["talib_c"]]) for title, g in GROUPS]
    groups = [(title, g) for title, g in groups if g]
    rows = [i for _, g in groups for i in g]
    if not rows:
        raise SystemExit("no overlapping indicators in the samples file")

    H = TOP + len(rows) * ROW_H + len(groups) * HEAD_H + 34
    p: list[str] = []
    add = p.append

    add(f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" '
        f'viewBox="0 0 {W} {H}" font-family="-apple-system,BlinkMacSystemFont,'
        f'Segoe UI,Helvetica,Arial,sans-serif" font-size="12">')
    add('<title>fugazi vs TA-Lib throughput, lower is better</title>')

    # Legend: two rows of two. Four on one line does not fit at this width.
    for i, (_, label, colour) in enumerate(SERIES):
        cx = LEFT + (i % 2) * 200
        cy = 14 + (i // 2) * 20
        add(f'<rect x="{cx}" y="{cy}" width="10" height="10" rx="2" fill="{colour}"/>')
        add(f'<text x="{cx + 16}" y="{cy + 9}" fill="{INK}">{label}</text>')
    # No "lower is better / n = …" caption in here. It collided with the legend
    # at the top and with the axis labels at the bottom, and as markdown beside
    # the image it is selectable, translatable and readable by a screen reader.

    # Plain gridlines. The 1.0x line used to be drawn dashed and highlighted as
    # "= TA-Lib C"; with the TA-Lib C bar itself in every row that was labelling
    # the same fact twice, so it is an ordinary gridline now.
    for g in range(1, int(XMAX) + 1):
        gx = x(g)
        add(f'<line x1="{gx:.1f}" y1="{TOP - 8}" x2="{gx:.1f}" y2="{H - 30}" '
            f'stroke="{INK}" stroke-width="0.5" opacity="0.35"/>')
        add(f'<text x="{gx:.1f}" y="{H - 14}" fill="{INK}" '
            f'text-anchor="middle">{g}.0x</text>')

    y0 = TOP - HEAD_H
    for title, group in groups:
        y0 += HEAD_H
        # Left-aligned from the plot's left edge, not right-aligned into the
        # label gutter: these titles are sentences, and anchoring them `end` at
        # `LEFT - 10` runs them off the left of the viewBox. The divider sits
        # above the title so the title reads as belonging to the rows below it.
        add(f'<line x1="{LEFT}" y1="{y0 - 18}" x2="{W - RIGHT}" y2="{y0 - 18}" '
            f'stroke="{INK}" stroke-width="0.5" opacity="0.25"/>')
        add(f'<text x="{LEFT}" y="{y0 - 5}" fill="{INK}" '
            f'font-size="11" opacity="0.85">{title}</text>')
        for name in group:
            render_row(add, stat, name, y0)
            y0 += ROW_H

    add('</svg>')

    with open(OUT, "w") as f:
        f.write("\n".join(p) + "\n")
    print(f"wrote {OUT} from {len(rows)} indicators, n = {n:,}")

    # Echo the table so the README can be updated from the same source of truth.
    print()
    hdr = f"{'indicator':10s}" + "".join(f"{label:>16s}" for _, label, _ in SERIES)
    print(hdr)
    for name in rows:
        line = f"{name:10s}"
        for key, _, _ in SERIES:
            line += f"{stat[key][name][0]:16.2f}" if name in stat[key] else f"{'--':>16s}"
        print(line)
    print()
    print(f"{'indicator':10s}{'rs vs C':>10s}{'py vs py':>10s}")
    for name in rows:
        c = stat["talib_c"][name][0]
        rs = stat["fugazi_rs"][name][0]
        tp = stat["talib_py"][name][0]
        fp = stat["fugazi_py"][name][0]
        print(f"{name:10s}{rs / c:9.2f}x{fp / tp:9.2f}x")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
