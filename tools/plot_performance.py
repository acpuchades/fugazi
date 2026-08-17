#!/usr/bin/env python3
"""Render the TA-Lib comparison as an SVG bar chart for the README.

Run after `pixi run -e bench bench`, with the numbers from that run pasted into
`ROWS` below:

    python3 tools/plot_performance.py

Why hand-rolled SVG rather than matplotlib: no dependency (this has to run from
a bare checkout), no fonts baked into paths, and full control over the two things
that actually matter for a README image —

  * **Dark mode.** GitHub serves the same file on a white and a near-black
    background, and an `<img>` cannot inherit the page's colours. So there is no
    background fill, and every label uses one mid-grey (`#8b949e`) that stays
    legible on both. Do not "fix" it to a darker grey; it will vanish for half
    the readers.
  * **Being diffable.** The input is a literal table and the output is text, so a
    number changing shows up in review as a number changing.

All four bars are normalised to **native TA-Lib C**, so every tier sits on one
scale and `1.0x` reads as "as fast as the C library". Note this is *not* the
like-for-like ratio for the Python bindings — a Python user should compare
against `talib`, which is the `py vs py` column of the README table. The chart
answers "where does everything sit?"; the table answers "what do I give up?".

`ROWS` holds **absolute ns/sample** and the ratios are derived here, on purpose:
hand-typed ratios are a second place for a number to be wrong.
"""

from __future__ import annotations

import os

OUT = os.path.join(
    os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
    "docs", "assets", "performance.svg",
)

# ns/sample: (indicator, TA-Lib C, fugazi rs, talib py, fugazi py).
# 200 000 samples, median of 7, best of 3 passes, from `pixi run -e bench bench`.
#
# The order is the point: each library is next to its own counterpart, so the
# two comparisons that mean something read straight off adjacent bars — native
# against native, then Python against Python. Interleaving them by language
# instead makes the reader do the pairing.
ROWS: list[tuple[str, float, float, float, float]] = [
    ("sma",    1.37, 1.37, 1.46, 4.97),
    ("ema",    2.06, 1.36, 2.16, 4.86),
    ("rsi",    4.79, 4.69, 4.98, 8.47),
    ("atr",    4.77, 4.54, 5.52, 36.56),
    ("stddev", 3.33, 10.61, 3.56, 12.77),
]

# Must stay aligned with `ROWS`' value order.
SERIES = [
    ("TA-Lib (C)", "#8b949e"),
    ("fugazi (Rust)", "#2f81f7"),
    ("talib (Python)", "#d29922"),
    ("fugazi (Python)", "#a371f7"),
]

W = 780
LEFT, RIGHT, TOP = 78, 40, 76
BAR_H, GAP = 11, 3
ROW_H = len(SERIES) * (BAR_H + GAP) + 15

INK = "#8b949e"          # legible on white and on #0d1117
MARK = "#d29922"

# Clipped rather than scaled to the maximum: `atr` through the Python bindings is
# 7.7x, and letting it set the axis would squash every other bar into
# illegibility. A clipped bar keeps its real number and gets a caret.
XMAX = 4.5
PLOT_W = W - LEFT - RIGHT
H = TOP + len(ROWS) * ROW_H + 34


def x(ratio: float) -> float:
    return LEFT + min(ratio, XMAX) / XMAX * PLOT_W


def main() -> int:
    os.makedirs(os.path.dirname(OUT), exist_ok=True)
    p: list[str] = []
    add = p.append

    add(f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" '
        f'viewBox="0 0 {W} {H}" font-family="-apple-system,BlinkMacSystemFont,'
        f'Segoe UI,Helvetica,Arial,sans-serif" font-size="12">')
    add('<title>fugazi vs TA-Lib throughput, lower is better</title>')

    # Legend: two rows of two. Four on one line does not fit at this width.
    for i, (label, colour) in enumerate(SERIES):
        cx = LEFT + (i % 2) * 200
        cy = 14 + (i // 2) * 20
        add(f'<rect x="{cx}" y="{cy}" width="10" height="10" rx="2" fill="{colour}"/>')
        add(f'<text x="{cx + 16}" y="{cy + 9}" fill="{INK}">{label}</text>')
    # No "lower is better / n = …" caption in here. It collided with the legend
    # at the top and with the axis labels at the bottom, and as markdown beside
    # the image it is selectable, translatable and readable by a screen reader.

    # Gridlines + x labels
    for g in (1, 2, 3, 4):
        gx = x(g)
        is_ref = g == 1
        add(f'<line x1="{gx:.1f}" y1="{TOP - 8}" x2="{gx:.1f}" y2="{H - 30}" '
            f'stroke="{MARK if is_ref else INK}" stroke-width="{1.5 if is_ref else 0.5}" '
            f'{"stroke-dasharray=\"4 3\"" if is_ref else "opacity=\"0.35\""}/>')
        label = "1.0x = TA-Lib C" if is_ref else f"{g}.0x"
        add(f'<text x="{gx:.1f}" y="{H - 14}" fill="{MARK if is_ref else INK}" '
            f'text-anchor="middle">{label}</text>')

    for i, (name, *vals) in enumerate(ROWS):
        y0 = TOP + i * ROW_H
        base = vals[0]
        add(f'<text x="{LEFT - 10}" y="{y0 + 2 * (BAR_H + GAP) + 2}" fill="{INK}" '
            f'text-anchor="end" font-family="ui-monospace,SFMono-Regular,'
            f'Consolas,monospace">{name}</text>')

        for j, (ns, (_, colour)) in enumerate(zip(vals, SERIES)):
            val = ns / base
            by = y0 + j * (BAR_H + GAP)
            w = max(x(val) - LEFT, 1.0)
            add(f'<rect x="{LEFT}" y="{by}" width="{w:.1f}" height="{BAR_H}" '
                f'rx="2" fill="{colour}"/>')
            # Clipped bars get a caret so a value past the axis reads as clipped
            # rather than as exactly 4.0.
            clipped = val > XMAX
            text = f'{val:.2f}x{" &#8250;" if clipped else ""}'
            # Inside the bar when it is wide enough, outside when it is not.
            # Outside-only put short bars' labels on top of the parity line.
            if w > 52:
                add(f'<text x="{LEFT + w - 5:.1f}" y="{by + BAR_H - 1}" '
                    f'fill="#ffffff" text-anchor="end" font-size="11">{text}</text>')
            else:
                add(f'<text x="{LEFT + w + 5:.1f}" y="{by + BAR_H - 1}" '
                    f'fill="{INK}" font-size="11">{text}</text>')

    add('</svg>')

    with open(OUT, "w") as f:
        f.write("\n".join(p) + "\n")
    print(f"wrote {OUT}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
